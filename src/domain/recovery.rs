//! Bounded compensation policy and exact, nonfatal normalization facts.
use super::{Codec, Timebase, TrackId};
use derive_more::Display;

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum RecoveryRejection {
    Disabled,
    UnsupportedConfiguration,
    UnrepresentableDuration,
    MaximumHole,
    MaximumCompensation,
    MaximumHoles,
}
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum RecoveryMethod {
    Gap,
    UnverifiedCadence,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryTransition {
    Degraded,
    Compensated,
    Recovered,
    Unavailable,
}

/// Totals count normalization, including samples a later pipeline failure may discard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompensationStatus {
    pub media_kind: super::MediaKind,
    pub cadence: Option<super::VideoCadence>,
    pub track: TrackId,
    pub codec: Codec,
    pub method: RecoveryMethod,
    pub timebase: Timebase,
    pub missing_ticks: u64,
    pub replacement_ticks: u64,
    pub episode_holes: u64,
    pub episode_ticks: u64,
    pub total_holes: u64,
    pub total_ticks: u64,
    pub degraded: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizationNotice {
    pub transition: RecoveryTransition,
    pub status: CompensationStatus,
}
