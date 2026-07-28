//! Deciding where segments and parts begin.
//!
//! Segmentation is delivery-agnostic: the plan describes cadence in each
//! track's own tick domain and says nothing about playlists. HLS and DASH both
//! consume it, and the muxer cuts to it.
//!
//! # The rule
//!
//! Pre-roll observes through the desired duration and chooses the latest
//! overlapping keyframe interval across every video track. If no compatible
//! interval exists, it expands the horizon only to the configured hard limit.
//! Audio snaps that video-prioritized instant to its own access-unit grid, so
//! codec priming and incompatible tick domains never invent a mid-unit cut.
//! Each rendition owns its resulting close-but-not-necessarily-equal cadence.

use std::{num::NonZero, time::Duration};

use thiserror::Error;

use crate::{
    domain::{TickDuration, TickTimestamp, Timebase, TrackId},
    media::{MediaError, PresentationPlan, SampleTimingError, TimelineCalibrationError},
};

mod cadence;
mod part;
mod preroll;

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
    /// Retained size, charged per [`NormalizedSample::retained_bytes`].
    ///
    /// [`NormalizedSample::retained_bytes`]: crate::media::NormalizedSample::retained_bytes
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
            maximum_buffered_bytes: 64 * 1024 * 1024,
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
    pub search: BoundarySearchPolicy,
}

impl SegmentationPolicy {
    /// Uses the latest compatible video cadence at or before the desired
    /// duration, extending by at most one more desired-duration window.
    ///
    /// The desired horizon is still decisive: extension is used only when one
    /// or more video tracks have not supplied a compatible keyframe cadence.
    pub fn latency_first(
        desired_segment_duration: Duration,
        desired_part_duration: Duration,
    ) -> Self {
        Self {
            desired_segment_duration,
            desired_part_duration,
            search: BoundarySearchPolicy::ExtendToNext {
                maximum_extension: desired_segment_duration,
            },
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

/// How far all video tracks may be observed for compatible keyframe cadences.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundarySearchPolicy {
    /// Select the latest compatible video boundary at or before the desired
    /// duration and reject if none exists.
    AtOrBeforeDesired,
    /// Search greedily beyond the desired duration, but never past this
    /// extension. The first compatible cross-video cluster resolves the plan.
    ExtendToNext { maximum_extension: Duration },
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
    /// Boundaries repeat as
    /// `first_segment_boundary_pts + k × segment_duration`. Neighboring tracks
    /// may differ by an access unit or video frame; the planner proves their
    /// first cadence intervals overlap rather than forcing equal tick values.
    pub segment_duration: NonZero<TickDuration>,
    /// How many presentable access units make up a regular part.
    ///
    /// Parts are counted rather than scheduled so that a regular part can
    /// never exceed [`Self::part_duration`], which HLS advertises as
    /// `PART-TARGET` and refuses to see exceeded. The final part of a segment
    /// holds whatever remains and may be shorter.
    pub part_access_units: NonZero<u32>,
    /// The longest a regular part may run: `part_access_units` access units at
    /// the longest access-unit duration observed for this track.
    pub part_duration: NonZero<TickDuration>,
    /// Maximum delay between a planned segment boundary and the next usable
    /// AU start.
    ///
    /// Video boundaries are selected random-access starts and therefore use
    /// zero; a muxer that waits for a late keyframe accounts for that from its
    /// own extension budget. Variable-duration audio may need up to one encoded
    /// frame when later access units do not repeat the pre-roll grid exactly.
    pub boundary_tolerance: TickDuration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentationPlan {
    tracks: Vec<TrackSegmentationPlan>,
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

        Ok(Self { tracks })
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
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PrerollError {
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
        TrackSegmentationPlan {
            track_id: TrackId(track_id),
            timebase: Timebase::hz90k(),
            presentation_origin_pts: 0,
            segmentation_origin_pts: 0,
            first_segment_boundary_pts: 180_000,
            segment_duration: nz::u64!(180_000),
            part_access_units: nz::u32!(6),
            part_duration: nz::u64!(18_000),
            boundary_tolerance: 0,
        }
    }

    #[test]
    fn default_segmentation_uses_apples_recommended_cadence() {
        let policy = SegmentationPolicy::default();

        assert_eq!(policy.desired_segment_duration, Duration::from_secs(6));
        assert_eq!(policy.desired_part_duration, Duration::from_secs(1));
        assert_eq!(
            policy.search,
            BoundarySearchPolicy::ExtendToNext {
                maximum_extension: Duration::from_secs(6)
            }
        );
    }

    #[test]
    fn segmentation_has_timing_for_every_track() {
        let plan = presentation(vec![
            track(0, MediaKind::Video),
            track(1, MediaKind::Video),
            track(2, MediaKind::Audio),
        ]);
        let segmentation = SegmentationPlan::new(
            &plan,
            vec![planned_track(0), planned_track(1), planned_track(2)],
        )
        .expect("segmentation plan is valid");

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
