use crate::{
    domain::{
        Appender, AudioTiming, Codec, DiscoveredTrack, MediaKind, MediaParameters, TickDuration,
        TickTimestamp, Timebase, TimebaseProjection, TimestampField, TimestampIssue,
        TimestampIssueCode, TrackId,
    },
    media::{AudioSample, NormalizeError, NormalizedMedia},
    source::Packet,
};

use super::{invalid_plan, processing, project_interval, project_timestamp};

pub(super) struct AudioNormalizer {
    pub(super) recovery: super::recovery::Recovery,
    track_id: TrackId,
    codec: Codec,
    projection: TimebaseProjection,
    timebase: Timebase,
    timestamp_tolerance: TickDuration,
    audible_start: TickTimestamp,
    frame_size: Option<TickDuration>,
    timing: AudioTiming,
    next_pts: Option<TickTimestamp>,
    first: bool,
    sample_rate: u32,
    /// A shortened fixed-frame packet is valid only if no successor exists.
    /// Holding that exceptional packet preserves strict mid-stream validation
    /// without adding one access unit of latency to ordinary audio.
    pending_terminal: Option<AudioSample>,
}

impl AudioNormalizer {
    pub(super) fn new(
        track: &DiscoveredTrack,
        output_timebase: Timebase,
        input_mode: crate::domain::InputMode,
    ) -> Result<Self, NormalizeError> {
        let MediaParameters::Audio {
            sample_rate,
            frame_size,
            timing,
            ..
        } = track.parameters
        else {
            return Err(invalid_plan(format!("{} is not an audio track", track.id)));
        };
        let projection = TimebaseProjection::new(track.timebase, output_timebase);
        let timestamp_tolerance = projection.one_source_tick_ceil().ok_or_else(|| {
            invalid_plan(format!("{} audio timestamp tolerance overflows", track.id))
        })?;
        Ok(Self {
            recovery: super::recovery::Recovery::new(track, output_timebase, input_mode),
            track_id: track.id,
            codec: track.codec,
            projection,
            timebase: output_timebase,
            // Enhanced RTMP Opus uses a 48 kHz clock to preserve pre-skip,
            // but its transport timestamps still have millisecond precision.
            timestamp_tolerance: if track.codec == Codec::Opus {
                timestamp_tolerance.max(48)
            } else {
                timestamp_tolerance
            },
            audible_start: track.first_pts.ok_or_else(|| {
                invalid_plan(format!("{} has no audible start timestamp", track.id))
            })?,
            frame_size: frame_size.map(|size| u64::from(size.get())),
            timing,
            next_pts: None,
            first: true,
            sample_rate: sample_rate.get(),
            pending_terminal: None,
        })
    }

    pub(super) fn track_id(&self) -> TrackId {
        self.track_id
    }

    pub(super) fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedMedia>,
    ) -> Result<(), NormalizeError> {
        if self.pending_terminal.take().is_some() {
            return Err(processing(format!(
                "{} shortened audio packet was followed by more audio",
                self.track_id
            )));
        }
        let (duration, inferred_trailing) = self.duration(&packet)?;
        let first = self.first;
        let (pts, repair) = self.packet_timing(&packet)?;
        let next_pts = pts
            .checked_add_unsigned(duration)
            .ok_or_else(|| processing(format!("{} audio clock overflows", self.track_id)))?;
        let mut trim = packet.audio_trim;
        if first && trim.leading_samples == 0 {
            trim.leading_samples = self.timing.initial_padding_samples;
        }
        if let Some(inferred) = inferred_trailing {
            trim.trailing_samples = self.terminal_trim(trim.trailing_samples, inferred)?;
        }
        // Validate every part of the real packet before emitting any repair.
        if self.codec == Codec::Opus && repair.is_some() {
            let decoded = crate::media::opus::packet_samples(packet.payload.as_bytes())
                .map_err(processing)?;
            self.ensure_duration_near(u64::from(decoded), duration, "Opus packet duration")?;
        }
        if let Some(repair) = &repair {
            out.push(NormalizedMedia::Gap(crate::media::MissingInterval {
                track_id: self.track_id,
                media_kind: MediaKind::Audio,
                start: self
                    .next_pts
                    .ok_or_else(|| processing("gap has no prior clock"))?,
                end: repair.end,
                timebase: self.timebase,
            }));
            self.recovery.commit(repair);
        }
        self.next_pts = Some(next_pts);
        self.first = false;
        self.recovery
            .real_audio(duration.saturating_sub(
                u64::from(trim.leading_samples) + u64::from(trim.trailing_samples),
            ));
        let sample = AudioSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            duration,
            trim,
            payload: packet.payload,
        };
        if inferred_trailing.is_some() {
            self.pending_terminal = Some(sample);
        } else {
            out.push(NormalizedMedia::Audio(sample));
        }
        Ok(())
    }

    fn packet_timing(
        &self,
        packet: &Packet,
    ) -> Result<(TickTimestamp, Option<super::recovery::Repair>), NormalizeError> {
        let supplied_pts = packet
            .pts
            .map(|pts| project_timestamp(self.projection, pts, self.track_id, "audio PTS"))
            .transpose()?;
        let supplied_dts = packet
            .dts
            .map(|dts| project_timestamp(self.projection, dts, self.track_id, "audio DTS"))
            .transpose()?;
        let first = self.first;
        let mut repair = None;
        let pts = if self.first {
            let audible_start = project_timestamp(
                self.projection,
                self.audible_start,
                self.track_id,
                "audible start",
            )?;
            let leading = self.first_packet_padding(packet)?;
            let encoded_start = audible_start
                .checked_sub_unsigned(leading)
                .ok_or_else(|| processing(format!("{} audio priming overflows", self.track_id)))?;
            if let Some(supplied) = supplied_pts {
                self.ensure_near(
                    supplied,
                    encoded_start,
                    TimestampField::Pts,
                    TimestampIssueCode::InitialTimestampMismatch,
                )?;
            }
            encoded_start
        } else {
            let expected = self.next_pts.ok_or_else(|| {
                processing(format!(
                    "{} audio clock has no next timestamp",
                    self.track_id
                ))
            })?;
            if let Some(supplied) = supplied_pts {
                // A contradictory DTS is malformed timing, not missing audio
                // that a codec-specific repair can safely replace.
                if i128::from(supplied) - i128::from(expected)
                    > i128::from(self.timestamp_tolerance)
                    && let Some(dts) = supplied_dts
                {
                    self.ensure_near(
                        dts,
                        supplied,
                        TimestampField::Dts,
                        TimestampIssueCode::AudioDtsMismatch,
                    )?;
                }
                let code = if supplied > expected {
                    TimestampIssueCode::AudioGap
                } else {
                    TimestampIssueCode::TimestampOverlap
                };
                if let Err(mut error) =
                    self.ensure_near(supplied, expected, TimestampField::Pts, code)
                {
                    if code != TimestampIssueCode::AudioGap {
                        return Err(error);
                    }
                    match self.recovery.prepare(expected, supplied) {
                        Ok(prepared) => repair = Some(prepared),
                        Err(reason) => {
                            if let NormalizeError::Timestamp(issue) = &mut error {
                                issue.recovery_rejection = Some(reason);
                            }
                            return Err(error);
                        }
                    }
                }
            }
            repair.as_ref().map_or(expected, |repair| repair.end)
        };
        if let Some(dts) = supplied_dts {
            let code = if first {
                TimestampIssueCode::InitialTimestampMismatch
            } else {
                TimestampIssueCode::AudioDtsMismatch
            };
            self.ensure_near(dts, pts, TimestampField::Dts, code)?;
        }
        Ok((pts, repair))
    }

    pub(super) fn finish(&mut self, out: &mut dyn Appender<NormalizedMedia>) {
        if let Some(sample) = self.pending_terminal.take() {
            out.push(NormalizedMedia::Audio(sample));
        }
    }

    fn first_packet_padding(&self, packet: &Packet) -> Result<TickDuration, NormalizeError> {
        let packet_padding = packet.audio_trim.leading_samples;
        let declared_padding = self.timing.initial_padding_samples;
        if packet_padding != 0 && declared_padding != 0 && packet_padding != declared_padding {
            return Err(processing(format!(
                "{} first packet declares {packet_padding} leading trim samples but its track declares {declared_padding} initial padding samples",
                self.track_id
            )));
        }
        Ok(TickDuration::from(if packet_padding != 0 {
            packet_padding
        } else {
            declared_padding
        }))
    }

    fn duration(&self, packet: &Packet) -> Result<(TickDuration, Option<u32>), NormalizeError> {
        if packet.duration.is_some_and(|duration| duration < 0) {
            return Err(processing(format!(
                "{} audio duration is negative",
                self.track_id
            )));
        }
        let supplied = packet
            .duration
            .filter(|duration| *duration > 0)
            .map(|duration| {
                let duration = TickDuration::try_from(duration).map_err(|_| {
                    processing(format!("{} audio duration is invalid", self.track_id))
                })?;
                let start = packet.pts.unwrap_or(0);
                project_interval(self.projection, start, duration, self.track_id)
                    .map(|(_, duration)| duration)
            })
            .transpose()?;
        if let Some(frame_size) = self.frame_size {
            if let Some(supplied) = supplied {
                let difference = supplied.abs_diff(frame_size);
                if difference > self.timestamp_tolerance {
                    if supplied < frame_size {
                        // The encoded access unit remains a complete codec
                        // frame; the shorter container interval describes how
                        // much of that frame belongs on the presentation tail.
                        let trailing = u32::try_from(frame_size - supplied).map_err(|_| {
                            processing(format!(
                                "{} terminal audio trim exceeds its frame size",
                                self.track_id
                            ))
                        })?;
                        return Ok((frame_size, Some(trailing)));
                    }
                    self.ensure_duration_near(supplied, frame_size, "audio packet duration")?;
                }
            }
            // Decoded samples are exact; a duration expressed in a coarse
            // container timebase is useful for validation but not for cadence.
            return Ok((frame_size, None));
        }
        supplied.map(|duration| (duration, None)).ok_or_else(|| {
            processing(format!(
                "{} has neither a fixed audio frame size nor a packet duration",
                self.track_id
            ))
        })
    }

    fn terminal_trim(&self, declared: u32, inferred: u32) -> Result<u32, NormalizeError> {
        if declared == 0 {
            return Ok(inferred);
        }
        if u64::from(declared).abs_diff(u64::from(inferred)) > self.timestamp_tolerance {
            return Err(processing(format!(
                "{} terminal audio trim disagrees with its packet duration",
                self.track_id
            )));
        }
        Ok(declared)
    }

    fn ensure_near(
        &self,
        actual: TickTimestamp,
        expected: TickTimestamp,
        field: TimestampField,
        code: TimestampIssueCode,
    ) -> Result<(), NormalizeError> {
        let distance = i128::from(actual)
            .checked_sub(i128::from(expected))
            .map_or(u128::MAX, i128::unsigned_abs);
        if distance > u128::from(self.timestamp_tolerance) {
            return Err(NormalizeError::Timestamp(Box::new(TimestampIssue {
                cadence: None,
                recovery_rejection: None,
                code,
                track: self.track_id,
                media_kind: MediaKind::Audio,
                codec: self.codec,
                field,
                reference: i128::from(expected),
                actual: i128::from(actual),
                timebase: self.timebase,
                tolerance_ticks: Some(self.timestamp_tolerance),
                maximum: None,
                // The difference between two i64 timestamps always fits u64.
                missing_ticks: (code == TimestampIssueCode::AudioGap)
                    .then(|| actual.abs_diff(expected)),
            })));
        }
        Ok(())
    }

    fn ensure_duration_near(
        &self,
        actual: TickDuration,
        expected: TickDuration,
        field: &'static str,
    ) -> Result<(), NormalizeError> {
        if actual.abs_diff(expected) > self.timestamp_tolerance {
            return Err(processing(format!(
                "{} {field} disagrees with its {} Hz sample clock",
                self.track_id, self.sample_rate
            )));
        }
        Ok(())
    }
}
