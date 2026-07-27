//! Codec and timing concerns: what may be presented, where the shared timeline
//! starts, and how raw packets become normalized access units.
//!
//! Validation and calibration are plain functions rather than injected traits.
//! Both are algorithms parameterized by data — a [`StreamPolicy`] and a track
//! catalog — so a seam would only let tests substitute something that is not
//! the thing running in production.

mod density;
mod normalize;
mod pacer;
mod sample;
mod stream;
mod timeline;
mod timing;
mod validate;

#[cfg(test)]
pub mod fixtures;

pub use density::{MediaDensityError, MediaDensityWindow};
pub use normalize::{
    MediaNormalizer, NormalizeError, NormalizerFactory, PassThroughNormalizerFactory,
    StartedNormalizer,
};
pub use pacer::{MediaPacer, PacingError};
pub use sample::{AudioSample, NormalizedSample, SubtitleSample, VideoSample};
pub use stream::{MediaError, SampleSource};
pub use timeline::{
    Rounding, TimelineCalibration, TimelineCalibrationError, TrackTimeline, calibrate,
};
pub use timing::{PresentedTiming, PresentedTimingCursor, SampleTimingError};
pub use validate::{PresentationPlan, ValidationError, validate};
