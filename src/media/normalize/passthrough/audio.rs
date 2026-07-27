use crate::{
    domain::{
        Appender, AudioTiming, Codec, DiscoveredTrack, MediaParameters, TickDuration,
        TickTimestamp, Timebase, TimebaseProjection, TrackId,
    },
    media::{AudioSample, NormalizeError, NormalizedSample},
    source::Packet,
};

use super::{invalid_plan, processing, project_interval, project_timestamp};

pub(super) struct AudioNormalizer {
    track_id: TrackId,
    codec: Codec,
    projection: TimebaseProjection,
    timestamp_tolerance: TickDuration,
    audible_start: TickTimestamp,
    frame_size: Option<TickDuration>,
    timing: AudioTiming,
    next_pts: Option<TickTimestamp>,
    first: bool,
    sample_rate: u32,
}

impl AudioNormalizer {
    pub(super) fn new(
        track: &DiscoveredTrack,
        output_timebase: Timebase,
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
            track_id: track.id,
            codec: track.codec,
            projection,
            timestamp_tolerance,
            audible_start: track.first_pts.ok_or_else(|| {
                invalid_plan(format!("{} has no audible start timestamp", track.id))
            })?,
            frame_size: frame_size.map(|size| u64::from(size.get())),
            timing,
            next_pts: None,
            first: true,
            sample_rate: sample_rate.get(),
        })
    }

    pub(super) fn track_id(&self) -> TrackId {
        self.track_id
    }

    pub(super) fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let duration = self.duration(&packet)?;
        let supplied_pts = packet
            .pts
            .map(|pts| project_timestamp(self.projection, pts, self.track_id, "audio PTS"))
            .transpose()?;
        let pts = if self.first {
            let audible_start = project_timestamp(
                self.projection,
                self.audible_start,
                self.track_id,
                "audible start",
            )?;
            let leading = self.first_packet_padding(&packet)?;
            let encoded_start = audible_start
                .checked_sub_unsigned(leading)
                .ok_or_else(|| processing(format!("{} audio priming overflows", self.track_id)))?;
            if let Some(supplied) = supplied_pts {
                self.ensure_near(supplied, encoded_start, "first audio PTS")?;
            }
            self.first = false;
            encoded_start
        } else {
            let expected = self.next_pts.ok_or_else(|| {
                processing(format!(
                    "{} audio clock has no next timestamp",
                    self.track_id
                ))
            })?;
            if let Some(supplied) = supplied_pts {
                self.ensure_near(supplied, expected, "audio PTS")?;
            }
            expected
        };
        if let Some(dts) = packet.dts {
            let dts = project_timestamp(self.projection, dts, self.track_id, "audio DTS")?;
            self.ensure_near(dts, pts, "audio DTS")?;
        }
        self.next_pts = Some(
            pts.checked_add_unsigned(duration)
                .ok_or_else(|| processing(format!("{} audio clock overflows", self.track_id)))?,
        );
        out.push(NormalizedSample::Audio(AudioSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            duration,
            trim: packet.audio_trim,
            payload: packet.payload,
        }));
        Ok(())
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

    fn duration(&self, packet: &Packet) -> Result<TickDuration, NormalizeError> {
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
                self.ensure_duration_near(supplied, frame_size, "audio packet duration")?;
            }
            // Decoded samples are exact; a duration expressed in a coarse
            // container timebase is useful for validation but not for cadence.
            return Ok(frame_size);
        }
        supplied.ok_or_else(|| {
            processing(format!(
                "{} has neither a fixed audio frame size nor a packet duration",
                self.track_id
            ))
        })
    }

    fn ensure_near(
        &self,
        actual: TickTimestamp,
        expected: TickTimestamp,
        field: &'static str,
    ) -> Result<(), NormalizeError> {
        let distance = i128::from(actual)
            .checked_sub(i128::from(expected))
            .map(i128::unsigned_abs)
            .unwrap_or(u128::MAX);
        if distance > u128::from(self.timestamp_tolerance) {
            return Err(processing(format!(
                "{} {field} discontinuity: expected {expected}, got {actual}",
                self.track_id
            )));
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
