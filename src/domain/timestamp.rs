//! Exact timing failures shared by normalization and host reporting.
use super::{Codec, MediaKind, Timebase, TrackId};
use derive_more::Display;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum TimestampIssueCode {
    VideoCadenceViolation,
    VideoCadenceUnavailable,
    VideoCadenceConflict,
    AudioGap,
    TimestampOverlap,
    AudioDtsMismatch,
    InitialTimestampMismatch,
    VideoTimestampOrder,
    VideoTimestampJump,
    VideoDurationLimit,
    VideoDtsMismatch,
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum TimestampField {
    Pts,
    Dts,
    Duration,
}

/// Values are in `timebase` ticks. Wide integers preserve differences at the
/// signed timestamp endpoints without rounding or saturation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "{track} {media_kind:?} {field} {code}: reference {reference}, got {actual} ({timebase:?})"
)]
pub struct TimestampIssue {
    pub cadence: Option<super::VideoCadence>,
    pub code: TimestampIssueCode,
    pub track: TrackId,
    pub media_kind: MediaKind,
    pub codec: Codec,
    pub field: TimestampField,
    pub reference: i128,
    pub actual: i128,
    pub timebase: Timebase,
    pub tolerance_ticks: Option<u64>,
    pub maximum: Option<Duration>,
    pub missing_ticks: Option<u64>,
    pub recovery_rejection: Option<super::RecoveryRejection>,
}
