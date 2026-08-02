//! Segmentation schedules for tests, with the uninteresting parts filled in.
//!
//! A [`TrackSegmentationPlan`] has nine fields and most tests care about one or
//! two of them. Spelling all nine at every call site buries the field under
//! test in boilerplate and, worse, makes an unrelated field easy to get subtly
//! wrong without any test noticing.
//!
//! The defaults describe the ordinary grid: a track that starts at the
//! presentation origin and whose first boundary sits exactly one segment after
//! it. A test about quantization overrides [`PlanBuilder::first_boundary`] to
//! sit deliberately off that grid, which is then visible as the one thing that
//! test changed.

use std::num::NonZero;

use crate::domain::{TickDuration, TickTimestamp, Timebase, TrackId};

use super::TrackSegmentationPlan;

/// Builds one track's schedule from the fields a test actually varies.
#[derive(Clone, Copy, Debug)]
pub struct PlanBuilder {
    plan: TrackSegmentationPlan,
    /// Whether a caller pinned the boundary, rather than taking the grid.
    ///
    /// Held separately so `segmentation_origin` and `first_boundary` may be
    /// set in either order without one silently overwriting the other.
    explicit_boundary: bool,
}

impl PlanBuilder {
    /// A track cut on `segment_duration`, one access unit per part.
    pub fn new(track_id: u32, timebase: Timebase, segment_duration: NonZero<TickDuration>) -> Self {
        Self {
            plan: TrackSegmentationPlan {
                track_id: TrackId(track_id),
                timebase,
                presentation_origin_pts: 0,
                segmentation_origin_pts: 0,
                first_segment_boundary_pts: 0,
                segment_duration,
                part_access_units: nz::u32!(1),
                part_duration: segment_duration,
                boundary_tolerance: 0,
            },
            explicit_boundary: false,
        }
    }

    /// How a regular part is measured: a count, and the ceiling it implies.
    pub fn part(mut self, access_units: NonZero<u32>, duration: NonZero<TickDuration>) -> Self {
        self.plan.part_access_units = access_units;
        self.plan.part_duration = duration;
        self
    }

    /// Where this track's PTS meets the publication's shared time anchor.
    pub fn presentation_origin(mut self, pts: TickTimestamp) -> Self {
        self.plan.presentation_origin_pts = pts;
        self
    }

    /// The encoded access-unit start segment zero is accounted from.
    pub fn segmentation_origin(mut self, pts: TickTimestamp) -> Self {
        self.plan.segmentation_origin_pts = pts;
        self
    }

    /// Pins the first boundary instead of taking one segment after the origin.
    pub fn first_boundary(mut self, pts: TickTimestamp) -> Self {
        self.plan.first_segment_boundary_pts = pts;
        self.explicit_boundary = true;
        self
    }

    /// How late a usable access-unit start may be without failing the plan.
    pub fn boundary_tolerance(mut self, ticks: TickDuration) -> Self {
        self.plan.boundary_tolerance = ticks;
        self
    }

    pub fn build(self) -> TrackSegmentationPlan {
        let mut plan = self.plan;
        if !self.explicit_boundary {
            plan.first_segment_boundary_pts = plan
                .segmentation_origin_pts
                .checked_add_unsigned(plan.segment_duration.get())
                .expect("a fixture schedule fits its tick domain");
        }
        plan
    }
}

/// Shorthand for the common case: one segment-length part, default grid.
pub fn plan(
    track_id: u32,
    timebase: Timebase,
    segment_duration: NonZero<TickDuration>,
) -> TrackSegmentationPlan {
    PlanBuilder::new(track_id, timebase, segment_duration).build()
}
