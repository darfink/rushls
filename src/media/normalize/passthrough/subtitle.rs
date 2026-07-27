use crate::{
    domain::{Appender, Codec, DiscoveredTrack, Timebase, TimebaseProjection, TrackId},
    media::{NormalizeError, NormalizedSample, SubtitleSample},
    source::Packet,
};

use super::{positive_duration, processing, project_interval};

pub(super) struct SubtitleNormalizer {
    track_id: TrackId,
    codec: Codec,
    projection: TimebaseProjection,
}

impl SubtitleNormalizer {
    pub(super) fn new(track: &DiscoveredTrack, output_timebase: Timebase) -> Self {
        Self {
            track_id: track.id,
            codec: track.codec,
            projection: TimebaseProjection::new(track.timebase, output_timebase),
        }
    }

    pub(super) fn track_id(&self) -> TrackId {
        self.track_id
    }

    pub(super) fn push(
        &self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        if !packet.audio_trim.is_empty() {
            return Err(processing(format!(
                "{} subtitle cue carries audio trim metadata",
                self.track_id
            )));
        }
        let pts = packet
            .pts
            .ok_or_else(|| processing(format!("{} subtitle cue has no PTS", self.track_id)))?;
        let duration = positive_duration(packet.duration, self.track_id, "subtitle duration")?;
        let (pts, duration) = project_interval(self.projection, pts, duration, self.track_id)?;
        out.push(NormalizedSample::Subtitle(SubtitleSample {
            track_id: self.track_id,
            codec: self.codec,
            pts,
            duration,
            webvtt: packet.webvtt,
            position: packet.subtitle_position,
            payload: packet.payload,
        }));
        Ok(())
    }
}
