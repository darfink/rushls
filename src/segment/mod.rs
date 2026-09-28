//! Deciding where segments and parts begin.
//!
//! Segmentation is delivery-agnostic: the plan describes cadence in each
//! track's own tick domain and says nothing about playlists. HLS and DASH both
//! consume it, and the muxer cuts to it.
//!
//! # The rule
//!
//! Pre-roll chooses the feasible common random-access instant closest to the
//! desired duration, preferring shorter durations on ties. Boundary evidence
//! may extend beyond the target, but the configured maximum remains a hard limit.
//! Audio snaps that video-prioritized instant to its own access-unit grid, so
//! codec priming and incompatible tick domains never invent a mid-unit cut.
//! Runtime coordination keeps video boundaries aligned across timebases.

use std::{num::NonZero, time::Duration};

use thiserror::Error;

use crate::{
    domain::{TickDuration, TickTimestamp, Timebase, TrackId},
    media::{MediaError, PresentationPlan, SampleTimingError, TimelineCalibrationError},
};

mod cadence;
pub mod cutter;
mod part;
mod preroll;
mod replay;

#[cfg(test)]
pub mod fixtures;

pub use cadence::CadenceObserver;
pub use preroll::{Preroll, PrerollRequest, run as run_preroll};

/// What pre-roll may spend before it gives up on a publisher.
///
/// Pre-roll is the one stage that retains everything it inspects — the samples
/// it examines are the ones the live tail replays, so nothing is discarded
/// until segmentation locks. All four bounds therefore matter, and none of them
/// subsumes the others: media duration is defeated by zero-duration access
/// units, byte budgets by empty payloads, and wall time by an input that floods
/// faster than it advances its clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrerollLimits {
    /// Retained size, charged per [`NormalizedMedia::retained_bytes`].
    ///
    /// [`NormalizedMedia::retained_bytes`]: crate::media::NormalizedMedia::retained_bytes
    pub maximum_buffered_bytes: usize,
    /// Retained sample count.
    ///
    /// Bounds the per-sample work of locking a plan, which the byte budget
    /// cannot: an input can stay far under budget while producing an enormous
    /// number of tiny access units.
    pub maximum_buffered_samples: usize,
    pub maximum_wall_time: Duration,
    pub maximum_media_duration: Duration,
}

impl PrerollLimits {
    /// Room for several seconds of high-bitrate multi-track media.
    pub fn permissive() -> Self {
        Self {
            maximum_buffered_bytes: crate::domain::PipelineBudget::DEFAULT_LIMIT,
            maximum_buffered_samples: 16_384,
            maximum_wall_time: Duration::from_secs(15),
            maximum_media_duration: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentationPolicy {
    pub desired_segment_duration: Duration,
    pub desired_part_duration: Duration,
    pub maximum_segment_duration: Duration,
    pub maximum_part_duration: Duration,
    pub early_boundary: Duration,
    pub late_boundary: Duration,
}

impl SegmentationPolicy {
    pub fn segment_cap(self) -> Duration {
        self.maximum_segment_duration
    }

    pub fn validate(self) -> Result<(), CadenceError> {
        let cap = self.segment_cap();
        if cap < self.desired_segment_duration
            || self.desired_segment_duration.is_zero()
            || self.desired_part_duration.is_zero()
            || self.desired_part_duration > self.desired_segment_duration
            || self.maximum_part_duration < self.desired_part_duration
            || self.maximum_part_duration > cap
            || self
                .early_boundary
                .checked_add(self.late_boundary)
                .is_none_or(|budget| budget >= cap)
        {
            return Err(CadenceError::InvalidPolicy);
        }
        Ok(())
    }

    /// Uses the feasible cadence closest to the desired duration, with a
    /// maximum of twice that duration. Shorter cadences win exact ties.
    pub fn latency_first(
        desired_segment_duration: Duration,
        desired_part_duration: Duration,
    ) -> Self {
        Self {
            desired_segment_duration,
            desired_part_duration,
            maximum_part_duration: desired_part_duration.saturating_mul(2),
            early_boundary: Duration::ZERO,
            late_boundary: Duration::ZERO,
            maximum_segment_duration: desired_segment_duration.saturating_mul(2),
        }
    }
}

impl Default for SegmentationPolicy {
    /// Apple's recommended HLS cadence: nominal six-second segments and
    /// one-second partial segments.
    fn default() -> Self {
        Self::latency_first(Duration::from_secs(6), Duration::from_secs(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackSegmentationPlan {
    pub track_id: TrackId,
    pub timebase: Timebase,
    /// Track-local PTS corresponding to the publication's shared time anchor.
    pub presentation_origin_pts: TickTimestamp,
    /// Encoded access-unit start from which segment zero is accounted.
    ///
    /// This may follow the shared origin when this track genuinely starts later.
    /// It may also precede the origin when the first audible sample lies inside
    /// an access unit: cuts must remain on encoded unit boundaries, while the
    /// edit list suppresses the leading codec priming.
    pub segmentation_origin_pts: TickTimestamp,
    /// Absolute selected boundary for closing segment zero.
    pub first_segment_boundary_pts: TickTimestamp,
    /// This track's selected segment cadence in its output time base.
    ///
    /// Integer nominal cadence. Fractional video periods use `segment_period`.
    /// Video renditions must match exact presentation instants across timebases.
    pub segment_duration: NonZero<TickDuration>,
    /// Exact video GOP period in track ticks, when declared timing agrees
    /// with the observed boundary to within one source container tick.
    pub segment_period: Option<(u64, u64)>,
    /// The immutable advertised part ceiling, checked against complete encoded
    /// access units by the same partitioner during admission and publication.
    pub part_duration: NonZero<TickDuration>,
    /// Maximum delay between a planned segment boundary and the next usable
    /// AU start.
    ///
    /// Video reserves container-clock quantization when frame timing requires
    /// rounding. Waiting for a late keyframe consumes the separate boundary
    /// window. Variable-duration audio may need up to one encoded
    /// frame when later access units do not repeat the pre-roll grid exactly.
    pub boundary_tolerance: TickDuration,
}

impl TrackSegmentationPlan {
    /// Both endpoints can move within the window. Audio rounds only upward;
    /// fractional video timestamps can quantize on either side of the grid.
    pub fn maximum_segment_ticks(self, allowances: Duration) -> u64 {
        let first = self
            .first_segment_boundary_pts
            .checked_sub(self.segmentation_origin_pts)
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(u64::MAX);
        self.segment_duration
            .get()
            .max(first)
            .saturating_add(
                self.boundary_tolerance
                    .saturating_mul(if self.segment_period.is_some() { 2 } else { 1 }),
            )
            .saturating_add(self.timebase.duration_to_ticks_floor(allowances))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentationPlan {
    tracks: Vec<TrackSegmentationPlan>,
    pub early_boundary: Duration,
    pub late_boundary: Duration,
    pub limits: PrerollLimits,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SegmentationPlanError {
    #[error("{0} occurs more than once in the segmentation plan")]
    DuplicateTrack(TrackId),
    #[error("segmentation plan references unknown {0}")]
    UnknownTrack(TrackId),
    #[error("no segmentation plan was provided for {0}")]
    MissingTrack(TrackId),
    #[error("segmentation and source timebases differ for {0}")]
    TimebaseMismatch(TrackId),
    #[error("segmentation timing is invalid for {0}")]
    InvalidTiming(TrackId),
}

impl SegmentationPlan {
    pub fn new(
        presentation: &PresentationPlan,
        tracks: Vec<TrackSegmentationPlan>,
    ) -> Result<Self, SegmentationPlanError> {
        for (index, track) in tracks.iter().enumerate() {
            if tracks[..index]
                .iter()
                .any(|candidate| candidate.track_id == track.track_id)
            {
                return Err(SegmentationPlanError::DuplicateTrack(track.track_id));
            }
            if presentation.catalog().get(track.track_id).is_none() {
                return Err(SegmentationPlanError::UnknownTrack(track.track_id));
            }
            let source = presentation
                .catalog()
                .get(track.track_id)
                .expect("the source track was checked above");
            if source.timebase != track.timebase {
                return Err(SegmentationPlanError::TimebaseMismatch(track.track_id));
            }
            let first_duration = track
                .first_segment_boundary_pts
                .checked_sub(track.segmentation_origin_pts)
                .and_then(|ticks| u64::try_from(ticks).ok());
            if first_duration.is_none_or(|duration| duration == 0)
                || track
                    .first_segment_boundary_pts
                    .checked_sub_unsigned(track.segment_duration.get())
                    .is_none()
            {
                return Err(SegmentationPlanError::InvalidTiming(track.track_id));
            }
        }

        for track in presentation.tracks() {
            if !tracks
                .iter()
                .any(|candidate| candidate.track_id == track.id)
            {
                return Err(SegmentationPlanError::MissingTrack(track.id));
            }
        }

        Ok(Self {
            tracks,
            early_boundary: Duration::ZERO,
            late_boundary: Duration::ZERO,
            limits: PrerollLimits::permissive(),
        })
    }

    pub fn get(&self, track_id: TrackId) -> Option<&TrackSegmentationPlan> {
        self.tracks.iter().find(|track| track.track_id == track_id)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &TrackSegmentationPlan> {
        self.tracks.iter()
    }

    /// The shortest regular part target across tracks, as wall-clock time.
    ///
    /// Health supervision uses this as the cadence a healthy session is
    /// expected to publish at.
    pub fn shortest_part_duration(&self) -> Duration {
        self.tracks
            .iter()
            .map(|track| track.timebase.ticks_to_duration(track.part_duration.get()))
            .min()
            .unwrap_or(Duration::ZERO)
    }

    /// The longest segment target across tracks, as wall-clock time.
    pub fn longest_segment_duration(&self) -> Duration {
        self.tracks
            .iter()
            .map(|track| {
                track
                    .timebase
                    .ticks_to_duration(track.segment_duration.get())
            })
            .max()
            .unwrap_or(Duration::ZERO)
    }
}

/// Why a schedule could not be derived from the media that was observed.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CadenceError {
    #[error("segmentation candidate search exceeded its work budget")]
    WorkLimitExceeded,
    #[error("invalid segmentation preferences, caps, or boundary allowances")]
    InvalidPolicy,
    #[error("timeline calibration contains duplicate {0}")]
    DuplicateTrack(TrackId),
    #[error("segmentation received an access unit for unknown {0}")]
    UnknownTrack(TrackId),
    #[error("access-unit timestamp overflowed for {0}")]
    TimestampOverflow(TrackId),
    /// Carries the underlying cause: an inexact trim, an overrun, and an
    /// arithmetic overflow are different operator problems, and collapsing
    /// them to one message loses the only detail that distinguishes them.
    #[error("{track_id} produced an access unit with unusable timing: {source}")]
    InvalidSampleTiming {
        track_id: TrackId,
        source: SampleTimingError,
    },
    #[error("segmentation search horizon cannot be represented for {0}")]
    HorizonOverflow(TrackId),
    #[error("track cadence comparison overflowed")]
    ComparisonOverflow,
    #[error("{0} presented no media to segment")]
    NoPresentableMedia(TrackId),
    #[error("{0} varies too much in access-unit duration to hold a part target")]
    InconsistentPartCadence(TrackId),
    #[error("no usable random-access boundary satisfies the segmentation policy")]
    NoSegmentationBoundary,
    #[error(
        "{tracks} video tracks have no common random-access boundary after startup within segment maximum {maximum:?}"
    )]
    UnalignedVideoBoundaries { tracks: usize, maximum: Duration },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PrerollError {
    #[error("admission timing failed: {0}")]
    Packaging(crate::mux::MuxError),
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    TimelineCalibration(#[from] TimelineCalibrationError),
    #[error(transparent)]
    SegmentationPlan(#[from] SegmentationPlanError),
    #[error(transparent)]
    Cadence(#[from] CadenceError),
    #[error("segmentation pre-roll exceeded its configured bound")]
    LimitExceeded,
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{MediaKind, fixtures::track},
        media::fixtures::presentation,
    };

    use super::*;

    fn planned_track(track_id: u32) -> TrackSegmentationPlan {
        fixtures::PlanBuilder::new(track_id, Timebase::hz90k(), nz::u64!(180_000))
            .part(nz::u32!(6), nz::u64!(18_000))
            .build()
    }

    #[test]
    fn default_segmentation_uses_apples_recommended_cadence() {
        let policy = SegmentationPolicy::default();

        assert_eq!(policy.desired_segment_duration, Duration::from_secs(6));
        assert_eq!(policy.desired_part_duration, Duration::from_secs(1));
        assert_eq!(policy.maximum_segment_duration, Duration::from_secs(12));
    }

    #[test]
    fn segmentation_has_timing_for_every_track() -> Result<(), SegmentationPlanError> {
        let plan = presentation(vec![
            track(0, MediaKind::Video),
            track(1, MediaKind::Video),
            track(2, MediaKind::Audio),
        ]);
        let segmentation = SegmentationPlan::new(
            &plan,
            vec![planned_track(0), planned_track(1), planned_track(2)],
        )?;

        assert_eq!(
            segmentation
                .get(TrackId(1))
                .map(|track| track.segment_duration),
            NonZero::new(180_000)
        );
        assert_eq!(
            segmentation.shortest_part_duration(),
            Duration::from_millis(200)
        );
        assert_eq!(
            segmentation.longest_segment_duration(),
            Duration::from_secs(2)
        );
        Ok(())
    }

    #[test]
    fn segmentation_rejects_duplicate_unknown_and_missing_tracks() {
        let plan = presentation(vec![track(0, MediaKind::Video), track(1, MediaKind::Audio)]);

        assert_eq!(
            SegmentationPlan::new(
                &plan,
                vec![planned_track(0), planned_track(0), planned_track(1)],
            ),
            Err(SegmentationPlanError::DuplicateTrack(TrackId(0)))
        );
        assert_eq!(
            SegmentationPlan::new(&plan, vec![planned_track(0), planned_track(7)],),
            Err(SegmentationPlanError::UnknownTrack(TrackId(7)))
        );
        assert_eq!(
            SegmentationPlan::new(&plan, vec![planned_track(0)],),
            Err(SegmentationPlanError::MissingTrack(TrackId(1)))
        );

        let mut mismatched = planned_track(0);
        mismatched.timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        assert_eq!(
            SegmentationPlan::new(&plan, vec![mismatched, planned_track(1)]),
            Err(SegmentationPlanError::TimebaseMismatch(TrackId(0)))
        );

        let mut invalid_timing = planned_track(0);
        invalid_timing.first_segment_boundary_pts = invalid_timing.segmentation_origin_pts;
        assert_eq!(
            SegmentationPlan::new(&plan, vec![invalid_timing, planned_track(1)]),
            Err(SegmentationPlanError::InvalidTiming(TrackId(0)))
        );

        let mut straddling = planned_track(1);
        straddling.presentation_origin_pts = 0;
        straddling.segmentation_origin_pts = -64;
        straddling.first_segment_boundary_pts = 180_000 - 64;
        assert!(
            SegmentationPlan::new(&plan, vec![planned_track(0), straddling]).is_ok(),
            "an edit list may suppress the part of an encoded unit before the origin"
        );
    }
}
