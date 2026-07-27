use std::{cmp::Ordering, ops::Range};

use super::{
    BoundaryAlignmentPolicy, BoundarySearchPolicy, BoundarySelection, BoundarySelectionError,
    TrackBoundary, TrackState,
};
use crate::domain::{MediaInstant, TickTimestamp};

#[derive(Clone, Copy, Debug)]
struct BoundaryCandidate {
    track_index: usize,
    pts: TickTimestamp,
}

/// Selects the best currently observed segment boundary for every track.
///
/// This function only ranks boundary evidence. The stateful
/// [`super::BoundarySelector`] remains responsible for deciding whether enough
/// media has been observed to make that evidence conclusive.
pub(super) fn select_segment_boundaries(
    tracks: &[TrackState],
    alignment: BoundaryAlignmentPolicy,
) -> Result<Option<BoundarySelection>, BoundarySelectionError> {
    let boundary_search = BoundarySearch { tracks };
    match alignment {
        BoundaryAlignmentPolicy::Aligned { search } => boundary_search.select_aligned(search),
        BoundaryAlignmentPolicy::Independent { search: policy } => {
            Ok(boundary_search.select_independent(policy))
        }
    }
}

/// Read-only search over the boundary evidence collected during pre-roll.
struct BoundarySearch<'a> {
    tracks: &'a [TrackState],
}

impl BoundarySearch<'_> {
    /// Selects one presentation instant covered by a boundary on every track.
    ///
    /// Prefer the latest common instant no later than the desired duration. If
    /// extension is allowed and no such instant exists, use the earliest common
    /// instant within the extension.
    fn select_aligned(
        &self,
        policy: BoundarySearchPolicy,
    ) -> Result<Option<BoundarySelection>, BoundarySelectionError> {
        let selected = self.latest_common_at_or_before_desired()?.or(match policy {
            BoundarySearchPolicy::AtOrBeforeDesired => None,
            BoundarySearchPolicy::ExtendToNext { .. } => self.earliest_common_after_desired()?,
        });
        let Some(selected) = selected else {
            return Ok(None);
        };

        let mut tracks = Vec::with_capacity(self.tracks.len());
        for (track_index, track) in self.tracks.iter().enumerate() {
            let boundary = self
                .boundary_covering_candidate(track_index, selected)?
                .expect("selected boundary is covered by every track");
            tracks.push(TrackBoundary {
                track_id: track.track_id,
                pts: boundary.start,
            });
        }
        Ok(Some(BoundarySelection::Aligned { tracks }))
    }

    /// Selects the preferred boundary on each track without cross-track
    /// compatibility checks.
    fn select_independent(&self, policy: BoundarySearchPolicy) -> Option<BoundarySelection> {
        let mut tracks = Vec::with_capacity(self.tracks.len());
        for track in self.tracks {
            let boundary = track
                .boundaries
                .iter()
                .rev()
                .find(|boundary| boundary.start <= track.desired_limit)
                .or_else(|| match policy {
                    BoundarySearchPolicy::AtOrBeforeDesired => None,
                    BoundarySearchPolicy::ExtendToNext { .. } => {
                        track.boundaries.iter().find(|boundary| {
                            track.desired_limit < boundary.start
                                && boundary.start <= track.maximum_limit
                        })
                    }
                })?;
            tracks.push(TrackBoundary {
                track_id: track.track_id,
                pts: boundary.start,
            });
        }
        Some(BoundarySelection::Independent { tracks })
    }

    fn latest_common_at_or_before_desired(
        &self,
    ) -> Result<Option<BoundaryCandidate>, BoundarySelectionError> {
        // The latest instant that still fits the desired duration: the segment
        // is as long as policy allows, so pre-roll waits no longer than it must.
        self.best_common(Ordering::Greater, |track, pts| {
            track.origin < pts && pts <= track.desired_limit
        })
    }

    fn earliest_common_after_desired(
        &self,
    ) -> Result<Option<BoundaryCandidate>, BoundarySelectionError> {
        // Nothing fit, and extension is permitted: take the first instant past
        // the desired duration, which costs the least added latency.
        self.best_common(Ordering::Less, |track, pts| {
            track.desired_limit < pts && pts <= track.maximum_limit
        })
    }

    /// The extreme candidate, in `prefer` order, that every track can meet.
    ///
    /// Both searches are this one: scan every track's boundaries, keep those
    /// the window admits, and take the furthest in the preferred direction that
    /// survives the cross-track check. Only the window and the direction differ,
    /// and writing them twice is how the two drift apart.
    ///
    /// Compatibility is checked last on purpose — it is the quadratic step, so
    /// a candidate that could not win anyway never pays for it.
    fn best_common(
        &self,
        prefer: Ordering,
        admits: impl Fn(&TrackState, TickTimestamp) -> bool,
    ) -> Result<Option<BoundaryCandidate>, BoundarySelectionError> {
        let mut selected: Option<BoundaryCandidate> = None;
        for (track_index, track) in self.tracks.iter().enumerate() {
            for boundary in &track.boundaries {
                let candidate = BoundaryCandidate {
                    track_index,
                    pts: boundary.start,
                };
                if !admits(track, candidate.pts) {
                    continue;
                }
                if let Some(current) = selected
                    && self.compare(candidate, current)? != prefer
                {
                    continue;
                }
                if self.is_common_to_all_tracks(candidate)? {
                    selected = Some(candidate);
                }
            }
        }
        Ok(selected)
    }

    fn is_common_to_all_tracks(
        &self,
        candidate: BoundaryCandidate,
    ) -> Result<bool, BoundarySelectionError> {
        for track_index in 0..self.tracks.len() {
            if self
                .boundary_covering_candidate(track_index, candidate)?
                .is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Finds the random-access AU whose presentation interval covers the
    /// candidate instant.
    ///
    /// Tracks can use different AU cadences and tick domains, so aligned
    /// boundaries need not have identical local start timestamps. Their
    /// half-open presentation intervals merely need to overlap at the selected
    /// instant.
    fn boundary_covering_candidate(
        &self,
        track_index: usize,
        candidate: BoundaryCandidate,
    ) -> Result<Option<&Range<TickTimestamp>>, BoundarySelectionError> {
        let track = &self.tracks[track_index];
        for boundary in &track.boundaries {
            let starts_before =
                self.compare_local(boundary.start, track_index, candidate)? != Ordering::Greater;
            let ends_after =
                self.compare_local(boundary.end, track_index, candidate)? == Ordering::Greater;
            if starts_before && ends_after {
                return Ok(Some(boundary));
            }
        }
        Ok(None)
    }

    /// Compares a track-local timestamp with a candidate from any track.
    fn compare_local(
        &self,
        local: TickTimestamp,
        track_index: usize,
        candidate: BoundaryCandidate,
    ) -> Result<Ordering, BoundarySelectionError> {
        self.compare(
            BoundaryCandidate {
                track_index,
                pts: local,
            },
            candidate,
        )
    }

    /// Compares presentation instants exactly, without converting either track
    /// into the other track's tick domain.
    fn compare(
        &self,
        left: BoundaryCandidate,
        right: BoundaryCandidate,
    ) -> Result<Ordering, BoundarySelectionError> {
        self.instant(left)
            .compare(self.instant(right))
            .ok_or(BoundarySelectionError::ComparisonOverflow)
    }

    fn instant(&self, candidate: BoundaryCandidate) -> MediaInstant {
        let track = &self.tracks[candidate.track_index];
        MediaInstant::new(track.timebase, candidate.pts, track.origin)
    }
}
