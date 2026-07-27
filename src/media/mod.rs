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
mod stream;
mod timeline;
mod validate;

#[cfg(test)]
pub mod fixtures;

pub use density::{MediaDensityError, MediaDensityWindow};
pub use normalize::{
    AudioSample, MediaNormalizer, NormalizeError, NormalizedSample, NormalizerFactory,
    SubtitleSample, VideoSample,
};
pub use pacer::{MediaPacer, PacingError};
pub use stream::{MediaError, SampleSource};
pub use timeline::{
    Rounding, TimelineCalibration, TimelineCalibrationError, TrackTimeline, calibrate,
};
pub use validate::{PresentationPlan, ValidationError, validate};
