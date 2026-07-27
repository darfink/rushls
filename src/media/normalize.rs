use thiserror::Error;

use crate::{
    domain::{Appender, Codec, Payload, TickDuration, TickTimestamp, TrackId},
    source::Packet,
};

use super::{PresentationPlan, TimelineCalibration};

/// An access unit normalized onto its calibrated track-local timeline.
///
/// PTS, DTS, and duration use the corresponding
/// [`TrackTimeline::timebase`](super::TrackTimeline); they are not implicitly
/// expressed in a global or canonical timebase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NormalizedSample {
    Video(VideoSample),
    Audio(AudioSample),
    Subtitle(SubtitleSample),
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
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleSample {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: TickTimestamp,
    pub duration: TickDuration,
    pub payload: Payload,
}

impl NormalizedSample {
    pub fn track_id(&self) -> TrackId {
        match self {
            Self::Video(sample) => sample.track_id,
            Self::Audio(sample) => sample.track_id,
            Self::Subtitle(sample) => sample.track_id,
        }
    }

    pub fn random_access(&self) -> bool {
        match self {
            Self::Video(sample) => sample.random_access,
            // Every audio access unit and subtitle cue is independently enterable.
            Self::Audio(_) | Self::Subtitle(_) => true,
        }
    }

    pub fn pts(&self) -> TickTimestamp {
        match self {
            Self::Video(sample) => sample.pts,
            Self::Audio(sample) => sample.pts,
            Self::Subtitle(sample) => sample.pts,
        }
    }

    pub fn duration(&self) -> TickDuration {
        match self {
            Self::Video(sample) => sample.duration,
            Self::Audio(sample) => sample.duration,
            Self::Subtitle(sample) => sample.duration,
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Self::Video(sample) => sample.payload.len(),
            Self::Audio(sample) => sample.payload.len(),
            Self::Subtitle(sample) => sample.payload.len(),
        }
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

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NormalizeError {
    #[error("cannot normalize the validated presentation: {0}")]
    InvalidPlan(Box<str>),
    #[error("media processing failed: {0}")]
    Processing(Box<str>),
}

/// Turns demultiplexed packets into calibrated access units.
///
/// Samples go to an [`Appender`] rather than into a generic sink type. That
/// single choice is what keeps this trait object-safe, and with it the entire
/// pipeline above: no type parameters propagate out of the hot path, and errors
/// stay flat instead of nesting one stage's failure inside the next one's. The
/// caller's buffer is reused across calls, so appending costs a move rather than
/// an allocation.
///
/// Expansion is bounded by
/// [`InputLimits::maximum_samples_per_batch`](crate::source::InputLimits::maximum_samples_per_batch),
/// measured across a whole batch of packets rather than per push, so holding
/// samples back for reordering and releasing them in a burst is fine.
pub trait MediaNormalizer: Send {
    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError>;

    /// Flushes any access unit still held for reordering or duration inference.
    ///
    /// Called both when the input ends normally and when a session is cut
    /// short, so it must complete promptly from state already in hand: it is
    /// synchronous precisely so it cannot wait for input that will never come.
    /// Repeated calls must be harmless and produce nothing further.
    fn finish(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), NormalizeError>;
}

/// Builds a normalizer for one validated, calibrated presentation.
///
/// Takes no meters: volume is counted by the loop that drives the normalizer,
/// once per batch, which is both cheaper and closer to the truth than having
/// each stage report itself.
pub trait NormalizerFactory: Send + Sync {
    fn start(
        &self,
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
    ) -> Result<Box<dyn MediaNormalizer>, NormalizeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_and_subtitles_are_intrinsically_random_access() {
        let audio = NormalizedSample::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: 90_000,
            duration: 1_920,
            payload: Payload::default(),
        });
        let subtitle = NormalizedSample::Subtitle(SubtitleSample {
            track_id: TrackId(2),
            codec: Codec::WebVtt,
            pts: 90_000,
            duration: 90_000,
            payload: Payload::default(),
        });

        assert!(audio.random_access());
        assert!(subtitle.random_access());
    }

    #[test]
    fn video_preserves_its_random_access_marker() {
        let video = NormalizedSample::Video(VideoSample {
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
