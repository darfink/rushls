//! Deciding where segments and parts begin.
//!
//! Segmentation is delivery-agnostic: the plan describes cadence in each
//! track's own tick domain and says nothing about playlists. HLS and DASH both
//! consume it, and the muxer cuts to it.

use std::{num::NonZero, time::Duration};

use thiserror::Error;

use crate::{
    domain::{TickDuration, Timebase, TrackId},
    media::{MediaError, PresentationPlan, TimelineCalibrationError},
};

mod boundary;
mod preroll;

pub use boundary::{
    BoundarySelection, BoundarySelectionError, BoundarySelectionStatus, BoundarySelector,
    TrackBoundary,
};
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
    pub alignment: BoundaryAlignmentPolicy,
}

impl SegmentationPolicy {
    /// Uses the latest aligned boundary at or before the desired duration.
    ///
    /// This is the recommended live default: normally aligned publishers incur
    /// no extra wait, while incompatible inputs are rejected instead of
    /// silently increasing latency.
    pub fn latency_first(
        desired_segment_duration: Duration,
        desired_part_duration: Duration,
    ) -> Self {
        Self {
            desired_segment_duration,
            desired_part_duration,
            alignment: BoundaryAlignmentPolicy::Aligned {
                search: BoundarySearchPolicy::AtOrBeforeDesired,
            },
        }
    }

    pub fn is_aligned(self) -> bool {
        matches!(self.alignment, BoundaryAlignmentPolicy::Aligned { .. })
    }
}

impl Default for SegmentationPolicy {
    /// Apple's recommended HLS cadence: nominal six-second segments and
    /// one-second partial segments, aligned across renditions.
    fn default() -> Self {
        Self::latency_first(Duration::from_secs(6), Duration::from_secs(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundarySearchPolicy {
    /// Select the latest usable random-access boundary at or before the
    /// desired duration. This bounds pre-roll latency and rejects inputs that
    /// cannot satisfy the requested alignment.
    AtOrBeforeDesired,
    /// Search beyond the desired duration for a usable boundary, but never
    /// beyond this extension. This accepts more inputs at the cost of latency.
    ExtendToNext { maximum_extension: Duration },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryAlignmentPolicy {
    /// All tracks use boundaries representing the same presentation time.
    ///
    /// This supports seamless rendition switching and Apple's aligned-boundary
    /// guidance. AU cadence can differ: each track maps the shared time to its
    /// own usable boundary rather than requiring numerically identical ticks.
    Aligned { search: BoundarySearchPolicy },
    /// Each track selects boundaries independently. This minimizes rejection
    /// and latency, but does not provide aligned rendition switching.
    Independent { search: BoundarySearchPolicy },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackSegmentationPlan {
    pub track_id: TrackId,
    pub timebase: Timebase,
    /// The selected segment cadence expressed in this track's output time base.
    pub segment_duration: NonZero<TickDuration>,
    /// Selected regular-part target expressed in this track's output time
    /// base. The final part of a segment may be shorter.
    pub part_duration: NonZero<TickDuration>,
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

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PrerollError {
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    TimelineCalibration(#[from] TimelineCalibrationError),
    #[error(transparent)]
    SegmentationPlan(#[from] SegmentationPlanError),
    #[error(transparent)]
    BoundarySelection(#[from] BoundarySelectionError),
    #[error("segmentation pre-roll exceeded its configured bound")]
    LimitExceeded,
    #[error("no usable random-access boundary satisfies the segmentation policy")]
    NoSegmentationBoundary,
    #[error("no usable part duration was found for {0}")]
    NoPartDuration(TrackId),
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
            segment_duration: nz::u64!(180_000),
            part_duration: nz::u64!(18_000),
        }
    }

    #[test]
    fn default_segmentation_uses_apples_recommended_cadence() {
        assert_eq!(
            SegmentationPolicy::default(),
            SegmentationPolicy::latency_first(Duration::from_secs(6), Duration::from_secs(1))
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
            SegmentationPlan::new(&plan, vec![planned_track(0), planned_track(7)]),
            Err(SegmentationPlanError::UnknownTrack(TrackId(7)))
        );
        assert_eq!(
            SegmentationPlan::new(&plan, vec![planned_track(0)]),
            Err(SegmentationPlanError::MissingTrack(TrackId(1)))
        );
    }
}
