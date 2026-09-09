use thiserror::Error;

use crate::{domain::Appender, source::Packet};

use super::{NormalizedSample, PresentationPlan, TimelineCalibration};

mod passthrough;

pub use passthrough::PassThroughNormalizerFactory;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NormalizeError {
    #[error(
        "{track}: unsupported random-access picture for {codec:?}; configure closed GOPs with IDR frames (AVC recovery-point SEI does not make an independent segment)"
    )]
    UnsupportedRandomAccess {
        track: crate::domain::TrackId,
        codec: crate::domain::Codec,
    },
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

/// A normalizer together with the representation it actually produces.
///
/// Timestamp normalization changes track timebases, so returning only the
/// processor would leave segmentation and muxing holding stale discovery
/// metadata. Startup is the atomic boundary where both become visible.
pub struct StartedNormalizer {
    pub normalizer: Box<dyn MediaNormalizer>,
    pub presentation: PresentationPlan,
    pub timeline: TimelineCalibration,
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
    ) -> Result<StartedNormalizer, NormalizeError>;
}
