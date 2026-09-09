use std::collections::VecDeque;

use crate::{
    domain::{
        Appender, Codec, DiscoveredTrack, FrameRate, RationalTickAccumulator, TickDuration,
        TickTimestamp, Timebase, TimebaseProjection, TrackId,
    },
    media::{NormalizeError, NormalizedSample, VideoSample},
    source::Packet,
};

use super::{invalid_plan, processing, project_interval, project_timestamp};

const MAXIMUM_VIDEO_REORDER_DEPTH: u32 = 64;

pub(super) struct VideoNormalizer {
    track_id: TrackId,
    codec: Codec,
    extradata: crate::domain::Payload,
    projection: TimebaseProjection,
    declared_clock: Option<RationalTickAccumulator>,
    video_delay: usize,
    held: Option<Packet>,
    pending_clock: VecDeque<TimedVideoPacket>,
    next_dts: Option<TickTimestamp>,
    last_duration: Option<TickDuration>,
    finished: bool,
}

struct TimedVideoPacket {
    packet: Packet,
    pts: TickTimestamp,
    duration: TickDuration,
}

impl VideoNormalizer {
    pub(super) fn new(
        track: &DiscoveredTrack,
        output_timebase: Timebase,
        frame_rate: Option<FrameRate>,
        video_delay: u32,
    ) -> Result<Self, NormalizeError> {
        if video_delay > MAXIMUM_VIDEO_REORDER_DEPTH {
            return Err(invalid_plan(format!(
                "{} declares video reorder depth {video_delay}, above {MAXIMUM_VIDEO_REORDER_DEPTH}",
                track.id
            )));
        }
        Ok(Self {
            track_id: track.id,
            codec: track.codec,
            extradata: track.codec_extradata.clone(),
            projection: TimebaseProjection::new(track.timebase, output_timebase),
            declared_clock: frame_rate
                .map(|rate| video_cadence(output_timebase, rate, track.id))
                .transpose()?,
            video_delay: video_delay as usize,
            held: None,
            pending_clock: VecDeque::with_capacity(video_delay as usize + 1),
            next_dts: None,
            last_duration: None,
            finished: false,
        })
    }

    pub(super) fn track_id(&self) -> TrackId {
        self.track_id
    }

    pub(super) fn push(
        &mut self,
        mut packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        if self.finished {
            return Err(processing(format!(
                "{} video packet arrived after finish",
                self.track_id
            )));
        }
        if packet.random_access
            && crate::media::video_config::closed_random_access(
                self.codec,
                self.extradata.as_bytes(),
                packet.payload.as_bytes(),
            ) == Some(false)
        {
            if self.codec == Codec::H264 {
                return Err(NormalizeError::UnsupportedRandomAccess {
                    track: self.track_id,
                    codec: self.codec,
                });
            }
            packet.random_access = false;
        }
        if packet.pts.is_none() {
            return Err(processing(format!(
                "{} video packet has no PTS",
                self.track_id
            )));
        }
        if !packet.audio_trim.is_empty() {
            return Err(processing(format!(
                "{} video packet carries audio trim metadata",
                self.track_id
            )));
        }
        if let Some(previous) = self.held.take() {
            let duration = self.resolve_duration(&previous, Some(&packet))?;
            self.accept_timed(previous, duration, out)?;
        }
        self.held = Some(packet);
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if let Some(packet) = self.held.take() {
            let duration = self.resolve_duration(&packet, None)?;
            self.accept_timed(packet, duration, out)?;
        }
        if !self.pending_clock.is_empty() {
            self.start_missing_clock(out)?;
        }
        Ok(())
    }

    fn resolve_duration(
        &mut self,
        packet: &Packet,
        next: Option<&Packet>,
    ) -> Result<TickDuration, NormalizeError> {
        if packet.duration.is_some_and(|duration| duration < 0) {
            return Err(processing(format!(
                "{} video duration is negative",
                self.track_id
            )));
        }
        // Advance the declared cadence for every access unit, including units
        // whose observed duration wins. If a later unit needs the fallback,
        // its fractional phase still corresponds to its real frame index.
        let declared = self
            .declared_clock
            .as_mut()
            .map(|clock| {
                clock
                    .advance(1)
                    .filter(|duration| *duration > 0)
                    .ok_or_else(|| {
                        processing(format!(
                            "{} frame duration cannot be represented",
                            self.track_id
                        ))
                    })
            })
            .transpose()?;
        // Validate the declared duration even when a timestamp step outranks
        // it, so a malformed one is still rejected rather than ignored.
        let declared_by_packet = packet
            .duration
            .filter(|duration| *duration > 0)
            .map(|duration| {
                TickDuration::try_from(duration)
                    .map_err(|_| processing(format!("{} video duration is invalid", self.track_id)))
            })
            .transpose()?
            .map(|duration| (packet.dts.or(packet.pts).unwrap_or(0), duration));
        // The distance to the next access unit outranks the declared duration.
        // Consecutive units have to tile the decode timeline exactly, and only
        // a timestamp step guarantees that: durations telescope back to the
        // source timestamps, so quantization stays bounded instead of
        // accumulating. A demuxer's `duration` is frequently a rounded
        // 1/frame_rate in a coarse container clock — FLV hands 24 fps over as
        // a flat 41 ms in its 1 kHz timebase, 0.67 ms short of the 42/41 ms
        // cadence its own timestamps carry, which is 16 ms of drift per second.
        let stepped_by_dts = next.and_then(|next| match (packet.dts, next.dts) {
            (Some(current), Some(following)) => following
                .checked_sub(current)
                .and_then(|duration| TickDuration::try_from(duration).ok())
                .filter(|duration| *duration > 0)
                .map(|duration| (current, duration)),
            _ => None,
        });
        // Without reordering, decode order is presentation order, so a PTS step
        // describes the decode timeline exactly as a DTS step would.
        let stepped_by_pts = (self.video_delay == 0).then_some(()).and_then(|()| {
            next.and_then(|next| {
                next.pts?
                    .checked_sub(packet.pts?)
                    .and_then(|duration| TickDuration::try_from(duration).ok())
                    .filter(|duration| *duration > 0)
                    .map(|duration| (packet.pts.expect("video PTS was validated"), duration))
            })
        });
        let source_interval = stepped_by_dts.or(stepped_by_pts).or(declared_by_packet);
        let observed = source_interval
            .map(|(start, duration)| {
                project_interval(self.projection, start, duration, self.track_id)
                    .map(|(_, duration)| duration)
            })
            .transpose()?;
        let duration = observed
            .or(declared)
            .or(self.last_duration)
            .ok_or_else(|| {
                processing(format!(
                    "{} video duration cannot be derived at end of input",
                    self.track_id
                ))
            })?;
        self.last_duration = Some(duration);
        Ok(duration)
    }

    fn accept_timed(
        &mut self,
        packet: Packet,
        duration: TickDuration,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let pts = project_timestamp(
            self.projection,
            packet.pts.expect("validated before the packet was held"),
            self.track_id,
            "video PTS",
        )?;
        let dts = packet
            .dts
            .map(|dts| project_timestamp(self.projection, dts, self.track_id, "video DTS"))
            .transpose()?;
        match (dts, self.next_dts) {
            (Some(anchor), Some(expected)) => {
                if anchor != expected {
                    return Err(processing(format!(
                        "{} video DTS discontinuity: expected {expected}, got {anchor}",
                        self.track_id
                    )));
                }
                self.emit(packet, pts, anchor, duration, out)?;
            }
            (Some(anchor), None) => {
                let pending_duration = self
                    .pending_clock
                    .iter()
                    .try_fold(0_u64, |sum, packet| sum.checked_add(packet.duration));
                let mut cursor = anchor
                    .checked_sub_unsigned(pending_duration.ok_or_else(|| {
                        processing(format!(
                            "{} pending video duration overflows",
                            self.track_id
                        ))
                    })?)
                    .ok_or_else(|| {
                        processing(format!("{} synthesized video DTS overflows", self.track_id))
                    })?;
                while let Some(pending) = self.pending_clock.pop_front() {
                    self.emit(pending.packet, pending.pts, cursor, pending.duration, out)?;
                    cursor = self.next_dts.expect("emission establishes the next DTS");
                }
                if cursor != anchor {
                    return Err(processing(format!(
                        "{} explicit video DTS does not follow buffered access units",
                        self.track_id
                    )));
                }
                self.emit(packet, pts, anchor, duration, out)?;
            }
            (None, Some(dts)) => self.emit(packet, pts, dts, duration, out)?,
            (None, None) => {
                self.pending_clock.push_back(TimedVideoPacket {
                    packet,
                    pts,
                    duration,
                });
                if self.pending_clock.len() > self.video_delay {
                    self.start_missing_clock(out)?;
                }
            }
        }
        Ok(())
    }

    fn start_missing_clock(
        &mut self,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let first_pts = self
            .pending_clock
            .front()
            .map(|packet| packet.pts)
            .ok_or_else(|| processing(format!("{} video clock has no packet", self.track_id)))?;
        let delay = self
            .pending_clock
            .iter()
            .take(self.video_delay)
            .try_fold(0_u64, |sum, packet| sum.checked_add(packet.duration));
        let mut cursor = first_pts
            .checked_sub_unsigned(delay.ok_or_else(|| {
                processing(format!("{} video reorder delay overflows", self.track_id))
            })?)
            .ok_or_else(|| {
                processing(format!("{} synthesized video DTS overflows", self.track_id))
            })?;
        while let Some(pending) = self.pending_clock.pop_front() {
            self.emit(pending.packet, pending.pts, cursor, pending.duration, out)?;
            cursor = self.next_dts.expect("emission establishes the next DTS");
        }
        Ok(())
    }

    fn emit(
        &mut self,
        packet: Packet,
        pts: TickTimestamp,
        dts: TickTimestamp,
        duration: TickDuration,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let next_dts = dts.checked_add_unsigned(duration).ok_or_else(|| {
            processing(format!("{} synthesized video DTS overflows", self.track_id))
        })?;
        self.next_dts = Some(next_dts);
        out.push(NormalizedSample::Video(VideoSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            dts,
            duration,
            random_access: packet.random_access,
            payload: packet.payload,
        }));
        Ok(())
    }
}

fn video_cadence(
    timebase: Timebase,
    frame_rate: FrameRate,
    track_id: TrackId,
) -> Result<RationalTickAccumulator, NormalizeError> {
    let frame_period = Timebase::new(frame_rate.denominator(), frame_rate.numerator());
    let clock = RationalTickAccumulator::requantize_to_ticks(frame_period, timebase)
        .ok_or_else(|| invalid_plan(format!("{track_id} frame cadence overflows")))?;
    let mut probe = clock;
    if probe.advance(1).is_none_or(|duration| duration == 0) {
        return Err(invalid_plan(format!(
            "{track_id} frame duration is not representable in its output timebase"
        )));
    }
    Ok(clock)
}

#[cfg(test)]
mod access_tests {
    use super::*;
    #[test]
    fn avc_open_gop_has_an_actionable_typed_failure() -> Result<(), NormalizeError> {
        let track = crate::domain::fixtures::TrackBuilder::new(0, crate::domain::MediaKind::Video)
            .codec_extradata(crate::mux::fixtures::H264_EXTRADATA)
            .build();
        let mut normalizer = VideoNormalizer::new(&track, track.timebase, None, 0)?;
        let packet = Packet {
            track_id: track.id,
            pts: Some(0),
            dts: Some(0),
            duration: Some(3_000),
            random_access: true,
            audio_trim: crate::domain::AudioTrim::default(),
            subtitle_position: None,
            webvtt: crate::domain::WebVttCueMetadata::default(),
            payload: crate::domain::Payload::from_bytes(bytes::Bytes::from_static(&[
                0, 0, 0, 2, 0x41, 0x80,
            ])),
        };
        let error = normalizer
            .push(packet, &mut Vec::new())
            .expect_err("non-IDR cannot start an independent AVC segment");
        assert!(matches!(
            error,
            NormalizeError::UnsupportedRandomAccess { .. }
        ));
        assert!(error.to_string().contains("closed GOPs with IDR"));
        Ok(())
    }
}
