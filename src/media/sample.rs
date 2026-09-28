use crate::domain::{
    AudioTrim, Codec, MediaKind, Payload, SubtitlePosition, TickDuration, TickTimestamp, Timebase,
    TrackId, WebVttCueMetadata,
};

/// An access unit or missing interval on its calibrated track-local timeline.
///
/// PTS, DTS, and duration use the corresponding
/// [`TrackTimeline::timebase`](super::TrackTimeline); they are not implicitly
/// expressed in a global or canonical timebase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NormalizedMedia {
    Video(VideoSample),
    Audio(AudioSample),
    Subtitle(SubtitleSample),
    Gap(MissingInterval),
}

/// Explicit absence on a calibrated track clock. End is exclusive.
/// This is ordered with real samples but never passed to a codec writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingInterval {
    pub track_id: TrackId,
    pub media_kind: MediaKind,
    pub start: TickTimestamp,
    pub end: TickTimestamp,
    pub timebase: Timebase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub dts: TickTimestamp,
    pub duration: TickDuration,
    pub random_access: bool,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub duration: TickDuration,
    pub trim: AudioTrim,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub duration: TickDuration,
    pub webvtt: WebVttCueMetadata,
    pub position: Option<SubtitlePosition>,
    pub payload: Payload,
}

impl NormalizedMedia {
    pub fn track_id(&self) -> TrackId {
        match self {
            Self::Video(sample) => sample.track_id,
            Self::Audio(sample) => sample.track_id,
            Self::Subtitle(sample) => sample.track_id,
            Self::Gap(gap) => gap.track_id,
        }
    }

    pub fn random_access(&self) -> bool {
        match self {
            Self::Video(sample) => sample.random_access,
            Self::Gap(_) => false,
            // Audio packet boundaries permit ordinary segmentation. The packager
            // checks decoder history before claiming part independence.
            Self::Audio(_) | Self::Subtitle(_) => true,
        }
    }

    pub fn pts(&self) -> TickTimestamp {
        match self {
            Self::Video(sample) => sample.pts,
            Self::Audio(sample) => sample.pts,
            Self::Subtitle(sample) => sample.pts,
            Self::Gap(gap) => gap.start,
        }
    }

    pub fn duration(&self) -> TickDuration {
        match self {
            Self::Video(sample) => sample.duration,
            Self::Audio(sample) => sample.duration,
            Self::Subtitle(sample) => sample.duration,
            Self::Gap(gap) => gap.end.abs_diff(gap.start),
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Self::Video(sample) => sample.payload.len(),
            Self::Audio(sample) => sample.payload.len(),
            Self::Gap(_) => 0,
            Self::Subtitle(sample) => sample
                .payload
                .len()
                .saturating_add(sample.webvtt.retained_bytes()),
        }
    }

    /// Account for a normalizer-created allocation before downstream retention.
    /// Payloads forwarded unchanged keep their existing allocation lease.
    pub fn account(
        &mut self,
        budget: Option<&crate::domain::PipelineBudget>,
    ) -> Result<(), crate::domain::BudgetExceeded> {
        let Some(budget) = budget else {
            return Ok(());
        };
        let overhead = size_of::<Self>();
        let payload = match self {
            Self::Video(sample) => &mut sample.payload,
            Self::Audio(sample) => &mut sample.payload,
            Self::Subtitle(sample) => &mut sample.payload,
            Self::Gap(_) => return Ok(()),
        };
        payload.account(budget, overhead, crate::domain::Stage::Normalization)
    }

    /// What holding this sample in a buffer actually costs.
    ///
    /// Charging only [`Self::payload_len`] would let an input with empty or
    /// tiny access units buffer without limit, because the per-sample struct —
    /// several timestamps, a codec, and a [`Payload`] handle — dominates at
    /// that size and would go uncounted. Callers enforcing a byte budget must
    /// use this rather than the payload length alone.
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.payload_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_and_subtitles_are_intrinsically_random_access() {
        let audio = NormalizedMedia::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: 90_000,
            duration: 1_920,
            trim: AudioTrim::default(),
            payload: Payload::default(),
        });
        let subtitle = NormalizedMedia::Subtitle(SubtitleSample {
            track_id: TrackId(2),
            codec: Codec::WebVtt,
            pts: 90_000,
            duration: 90_000,
            webvtt: WebVttCueMetadata::default(),
            position: None,
            payload: Payload::default(),
        });

        assert!(audio.random_access());
        assert!(subtitle.random_access());
    }

    #[test]
    fn video_preserves_its_random_access_marker() {
        let video = NormalizedMedia::Video(VideoSample {
            track_id: TrackId(0),
            codec: Codec::H264,
            pts: 90_000,
            dts: 90_000,
            duration: 3_000,
            random_access: false,
            payload: Payload::default(),
        });

        assert!(!video.random_access());
    }
}
