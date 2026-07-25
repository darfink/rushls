use std::{ops::Range, time::Duration};

use thiserror::Error;

mod part;
mod segment;

use super::{BoundaryAlignmentPolicy, BoundarySearchPolicy, SegmentationPolicy};
use crate::{
    domain::{TickTimestamp, Timebase, TrackId},
    media::{NormalizedSample, TimelineCalibration},
};
pub(super) use part::select_part_duration;
use segment::select_segment_boundaries;

#[derive(Debug)]
struct TrackState {
    /// Stable identity used to route normalized AUs into this state.
    track_id: TrackId,
    /// Tick domain of this track's normalized AUs.
    timebase: Timebase,
    /// Track-local PTS representing the shared presentation origin.
    origin: TickTimestamp,
    /// Latest boundary allowed by the desired duration, rounded down so a
    /// selected boundary never exceeds the policy.
    desired_limit: TickTimestamp,
    /// Observation horizon rounded up so pre-roll consumes at least through
    /// the desired duration before searching backward.
    desired_coverage: TickTimestamp,
    /// Latest boundary allowed when bounded extension is enabled.
    maximum_limit: TickTimestamp,
    /// Terminal observation horizon for proving that extended search failed.
    maximum_coverage: TickTimestamp,
    /// Greatest observed AU end PTS in this track's tick domain.
    observed_until: Option<TickTimestamp>,
    /// Random-access AU ranges retained for boundary compatibility searches.
    boundaries: Vec<Range<TickTimestamp>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackBoundary {
    pub track_id: TrackId,
    /// Boundary PTS expressed in this track's normalized timebase.
    pub pts: TickTimestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundarySelection {
    /// Boundaries represent the same presentation instant but retain their
    /// respective track-local tick values.
    Aligned {
        tracks: Vec<TrackBoundary>,
    },
    Independent {
        tracks: Vec<TrackBoundary>,
    },
}

impl BoundarySelection {
    pub fn tracks(&self) -> &[TrackBoundary] {
        match self {
            Self::Aligned { tracks } | Self::Independent { tracks } => tracks,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundarySelectionStatus {
    Pending,
    Ready(BoundarySelection),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum BoundarySelectionError {
    #[error("timeline calibration contains duplicate {0}")]
    DuplicateTrack(TrackId),
    #[error("boundary selector received an access unit for unknown {0}")]
    UnknownTrack(TrackId),
    #[error("access-unit timestamp overflowed for {0}")]
    TimestampOverflow(TrackId),
    #[error("segmentation search horizon cannot be represented for {0}")]
    HorizonOverflow(TrackId),
    #[error("segmentation search durations cannot be represented")]
    DurationOverflow,
    #[error("cross-track timestamp comparison overflowed")]
    ComparisonOverflow,
    #[error("{0} offered more than {MAXIMUM_BOUNDARIES_PER_TRACK} candidate boundaries")]
    TooManyBoundaries(TrackId),
    #[error("no usable random-access boundary satisfies the segmentation policy")]
    NoCompatibleBoundary,
}

/// Candidate boundaries retained per track before the input is rejected.
///
/// Cross-track compatibility is a pairwise search, so the cost of a selection
/// attempt is quadratic in this number. An all-intra 60fps feed offers roughly
/// 60 candidates per second, which puts this cap around a minute of media —
/// far beyond any pre-roll horizon a sane policy configures. Reaching it means
/// the input is emitting zero-duration or wildly out-of-order access units, and
/// the honest response is to reject the publisher rather than to keep searching.
const MAXIMUM_BOUNDARIES_PER_TRACK: usize = 4_096;

/// Selects segmentation boundaries from normalized, track-local access units.
///
/// Each track retains its natural normalized timebase (for example 90 kHz
/// video and sample-rate audio). Cross-track alignment uses exact rational
/// comparisons; it never requantizes AU timestamps through a canonical clock.
///
/// State is allocated once per calibrated track. The hot `observe` path only
/// appends random-access ranges; ordinary access units update track progress
/// without allocating.
pub struct BoundarySelector {
    alignment: BoundaryAlignmentPolicy,
    tracks: Vec<TrackState>,
}

impl BoundarySelector {
    pub fn new(
        timeline: &TimelineCalibration,
        policy: SegmentationPolicy,
    ) -> Result<Self, BoundarySelectionError> {
        let extension = search_extension(policy.alignment);
        let maximum_duration = policy
            .desired_segment_duration
            .checked_add(extension)
            .ok_or(BoundarySelectionError::DurationOverflow)?;
        let mut tracks = Vec::with_capacity(timeline.tracks.len());

        for track in &timeline.tracks {
            if tracks
                .iter()
                .any(|candidate: &TrackState| candidate.track_id == track.track_id)
            {
                return Err(BoundarySelectionError::DuplicateTrack(track.track_id));
            }

            let desired_limit = track
                .origin_pts
                .checked_add_unsigned(
                    track
                        .timebase
                        .duration_to_ticks_floor(policy.desired_segment_duration),
                )
                .ok_or(BoundarySelectionError::HorizonOverflow(track.track_id))?;
            let desired_coverage = track
                .origin_pts
                .checked_add_unsigned(
                    track
                        .timebase
                        .duration_to_ticks_ceil(policy.desired_segment_duration),
                )
                .ok_or(BoundarySelectionError::HorizonOverflow(track.track_id))?;
            let maximum_limit = track
                .origin_pts
                .checked_add_unsigned(track.timebase.duration_to_ticks_floor(maximum_duration))
                .ok_or(BoundarySelectionError::HorizonOverflow(track.track_id))?;
            let maximum_coverage = track
                .origin_pts
                .checked_add_unsigned(track.timebase.duration_to_ticks_ceil(maximum_duration))
                .ok_or(BoundarySelectionError::HorizonOverflow(track.track_id))?;

            tracks.push(TrackState {
                track_id: track.track_id,
                timebase: track.timebase,
                origin: track.origin_pts,
                desired_limit,
                desired_coverage,
                maximum_limit,
                maximum_coverage,
                observed_until: None,
                boundaries: Vec::new(),
            });
        }

        Ok(Self {
            alignment: policy.alignment,
            tracks,
        })
    }

    pub fn observe(&mut self, sample: &NormalizedSample) -> Result<(), BoundarySelectionError> {
        let track_id = sample.track_id();
        // Validation keeps the track set small. During bounded pre-roll a
        // cache-friendly linear scan is cheaper than maintaining a hash index.
        let track = self
            .tracks
            .iter_mut()
            .find(|track| track.track_id == track_id)
            .ok_or(BoundarySelectionError::UnknownTrack(track_id))?;
        let end = sample
            .pts()
            .checked_add_unsigned(sample.duration())
            .ok_or(BoundarySelectionError::TimestampOverflow(track_id))?;

        track.observed_until = Some(
            track
                .observed_until
                .map_or(end, |observed| observed.max(end)),
        );
        if sample.random_access() && sample.pts() > track.origin {
            if track.boundaries.len() >= MAXIMUM_BOUNDARIES_PER_TRACK {
                return Err(BoundarySelectionError::TooManyBoundaries(track_id));
            }
            track.boundaries.push(sample.pts()..end);
        }
        Ok(())
    }

    pub fn selection(&self) -> Result<BoundarySelectionStatus, BoundarySelectionError> {
        if !self.desired_horizon_covered() {
            return Ok(BoundarySelectionStatus::Pending);
        }

        let selection = select_segment_boundaries(&self.tracks, self.alignment)?;
        if let Some(selection) = selection {
            return Ok(BoundarySelectionStatus::Ready(selection));
        }
        if self.maximum_horizon_covered() {
            return Err(BoundarySelectionError::NoCompatibleBoundary);
        }
        Ok(BoundarySelectionStatus::Pending)
    }
}

impl BoundarySelector {
    fn desired_horizon_covered(&self) -> bool {
        self.tracks.iter().all(|track| {
            track
                .observed_until
                .is_some_and(|time| time >= track.desired_coverage)
        })
    }

    fn maximum_horizon_covered(&self) -> bool {
        self.tracks.iter().all(|track| {
            track
                .observed_until
                .is_some_and(|time| time >= track.maximum_coverage)
        })
    }
}

fn search_extension(policy: BoundaryAlignmentPolicy) -> Duration {
    match policy {
        BoundaryAlignmentPolicy::Aligned { search }
        | BoundaryAlignmentPolicy::Independent { search } => match search {
            BoundarySearchPolicy::AtOrBeforeDesired => Duration::ZERO,
            BoundarySearchPolicy::ExtendToNext { maximum_extension } => maximum_extension,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        domain::{Codec, Payload, Timebase, TrackId},
        media::{AudioSample, TrackTimeline, VideoSample},
    };

    const VIDEO_SECOND: i64 = 90_000;
    const AUDIO_SECOND: i64 = 48_000;

    fn timeline() -> TimelineCalibration {
        TimelineCalibration {
            timing_authority: TrackId(0),
            tracks: vec![
                TrackTimeline {
                    track_id: TrackId(0),
                    timebase: Timebase::hz90k(),
                    origin_pts: 0,
                },
                TrackTimeline {
                    track_id: TrackId(1),
                    timebase: Timebase::new(nz::u32!(1), nz::u32!(48_000)),
                    origin_pts: 0,
                },
            ],
        }
    }

    fn selector(alignment: BoundaryAlignmentPolicy) -> BoundarySelector {
        BoundarySelector::new(
            &timeline(),
            SegmentationPolicy {
                desired_segment_duration: Duration::from_secs(10),
                desired_part_duration: Duration::from_millis(200),
                alignment,
            },
        )
        .expect("test policy is representable")
    }

    fn at_or_before(aligned: bool) -> BoundaryAlignmentPolicy {
        let search = BoundarySearchPolicy::AtOrBeforeDesired;
        if aligned {
            BoundaryAlignmentPolicy::Aligned { search }
        } else {
            BoundaryAlignmentPolicy::Independent { search }
        }
    }

    fn extend(aligned: bool) -> BoundaryAlignmentPolicy {
        let search = BoundarySearchPolicy::ExtendToNext {
            maximum_extension: Duration::from_secs(5),
        };
        if aligned {
            BoundaryAlignmentPolicy::Aligned { search }
        } else {
            BoundaryAlignmentPolicy::Independent { search }
        }
    }

    fn observe(
        selector: &mut BoundarySelector,
        track_id: u32,
        start_seconds: i64,
        duration_seconds: u64,
        random_access: bool,
    ) {
        let sample = if track_id == 0 {
            NormalizedSample::Video(VideoSample {
                track_id: TrackId(track_id),
                codec: Codec::H264,
                pts: start_seconds * VIDEO_SECOND,
                dts: start_seconds * VIDEO_SECOND,
                duration: duration_seconds * VIDEO_SECOND as u64,
                random_access,
                payload: Payload::default(),
            })
        } else {
            NormalizedSample::Audio(AudioSample {
                track_id: TrackId(track_id),
                codec: Codec::Aac,
                pts: start_seconds * AUDIO_SECOND,
                duration: duration_seconds * AUDIO_SECOND as u64,
                payload: Payload::default(),
            })
        };
        selector
            .observe(&sample)
            .expect("test sample belongs to the timeline");
    }

    fn cover(selector: &mut BoundarySelector, through_seconds: i64) {
        observe(selector, 0, through_seconds - 1, 1, false);
        observe(selector, 1, through_seconds - 1, 1, false);
    }

    #[test]
    fn aligned_at_or_before_selects_track_local_boundaries() {
        let mut selector = selector(at_or_before(true));
        observe(&mut selector, 0, 8, 1, true);
        observe(&mut selector, 1, 8, 1, true);
        cover(&mut selector, 10);

        assert_eq!(
            selector.selection(),
            Ok(BoundarySelectionStatus::Ready(BoundarySelection::Aligned {
                tracks: vec![
                    TrackBoundary {
                        track_id: TrackId(0),
                        pts: 8 * VIDEO_SECOND,
                    },
                    TrackBoundary {
                        track_id: TrackId(1),
                        pts: 8 * AUDIO_SECOND,
                    },
                ],
            }))
        );
    }

    #[test]
    fn aligned_comparison_accounts_for_each_tracks_local_origin() {
        let timeline = TimelineCalibration {
            timing_authority: TrackId(0),
            tracks: vec![
                TrackTimeline {
                    track_id: TrackId(0),
                    timebase: Timebase::hz90k(),
                    origin_pts: VIDEO_SECOND,
                },
                TrackTimeline {
                    track_id: TrackId(1),
                    timebase: Timebase::new(nz::u32!(1), nz::u32!(48_000)),
                    origin_pts: 2 * AUDIO_SECOND,
                },
            ],
        };
        let mut selector = BoundarySelector::new(
            &timeline,
            SegmentationPolicy::latency_first(Duration::from_secs(10), Duration::from_millis(200)),
        )
        .expect("test policy is representable");
        selector
            .observe(&NormalizedSample::Video(VideoSample {
                track_id: TrackId(0),
                codec: Codec::H264,
                pts: 9 * VIDEO_SECOND,
                dts: 9 * VIDEO_SECOND,
                duration: VIDEO_SECOND as u64,
                random_access: true,
                payload: Payload::default(),
            }))
            .expect("video track is calibrated");
        selector
            .observe(&NormalizedSample::Audio(AudioSample {
                track_id: TrackId(1),
                codec: Codec::Aac,
                pts: 10 * AUDIO_SECOND,
                duration: AUDIO_SECOND as u64,
                payload: Payload::default(),
            }))
            .expect("audio track is calibrated");
        selector
            .observe(&NormalizedSample::Video(VideoSample {
                track_id: TrackId(0),
                codec: Codec::H264,
                pts: 10 * VIDEO_SECOND,
                dts: 10 * VIDEO_SECOND,
                duration: VIDEO_SECOND as u64,
                random_access: false,
                payload: Payload::default(),
            }))
            .expect("video track is calibrated");
        selector
            .observe(&NormalizedSample::Audio(AudioSample {
                track_id: TrackId(1),
                codec: Codec::Aac,
                pts: 11 * AUDIO_SECOND,
                duration: AUDIO_SECOND as u64,
                payload: Payload::default(),
            }))
            .expect("audio track is calibrated");

        assert_eq!(
            selector.selection(),
            Ok(BoundarySelectionStatus::Ready(BoundarySelection::Aligned {
                tracks: vec![
                    TrackBoundary {
                        track_id: TrackId(0),
                        pts: 9 * VIDEO_SECOND,
                    },
                    TrackBoundary {
                        track_id: TrackId(1),
                        pts: 10 * AUDIO_SECOND,
                    },
                ],
            }))
        );
    }

    #[test]
    fn aligned_at_or_before_rejects_when_no_common_boundary_exists() {
        let mut selector = selector(at_or_before(true));
        observe(&mut selector, 0, 8, 1, true);
        observe(&mut selector, 1, 7, 1, true);
        cover(&mut selector, 10);

        assert_eq!(
            selector.selection(),
            Err(BoundarySelectionError::NoCompatibleBoundary)
        );
    }

    #[test]
    fn aligned_extension_selects_next_common_boundary() {
        let mut selector = selector(extend(true));
        observe(&mut selector, 0, 12, 1, true);
        observe(&mut selector, 1, 12, 1, true);

        assert!(matches!(
            selector.selection(),
            Ok(BoundarySelectionStatus::Ready(
                BoundarySelection::Aligned { .. }
            ))
        ));
    }

    #[test]
    fn aligned_extension_rejects_after_the_search_bound() {
        let mut selector = selector(extend(true));
        observe(&mut selector, 0, 12, 1, true);
        observe(&mut selector, 1, 13, 1, true);
        cover(&mut selector, 15);

        assert_eq!(
            selector.selection(),
            Err(BoundarySelectionError::NoCompatibleBoundary)
        );
    }

    #[test]
    fn independent_at_or_before_selects_each_tracks_latest_boundary() {
        let mut selector = selector(at_or_before(false));
        observe(&mut selector, 0, 8, 1, true);
        observe(&mut selector, 1, 7, 1, true);
        cover(&mut selector, 10);

        assert_eq!(
            selector.selection(),
            Ok(BoundarySelectionStatus::Ready(
                BoundarySelection::Independent {
                    tracks: vec![
                        TrackBoundary {
                            track_id: TrackId(0),
                            pts: 8 * VIDEO_SECOND,
                        },
                        TrackBoundary {
                            track_id: TrackId(1),
                            pts: 9 * AUDIO_SECOND,
                        },
                    ],
                }
            ))
        );
    }

    #[test]
    fn independent_at_or_before_rejects_when_a_track_has_no_boundary() {
        let mut selector = selector(at_or_before(false));
        cover(&mut selector, 10);

        assert_eq!(
            selector.selection(),
            Err(BoundarySelectionError::NoCompatibleBoundary)
        );
    }

    #[test]
    fn independent_extension_selects_each_tracks_next_boundary() {
        let mut selector = selector(extend(false));
        observe(&mut selector, 0, 12, 1, true);
        observe(&mut selector, 1, 13, 1, true);

        assert_eq!(
            selector.selection(),
            Ok(BoundarySelectionStatus::Ready(
                BoundarySelection::Independent {
                    tracks: vec![
                        TrackBoundary {
                            track_id: TrackId(0),
                            pts: 12 * VIDEO_SECOND,
                        },
                        TrackBoundary {
                            track_id: TrackId(1),
                            pts: 13 * AUDIO_SECOND,
                        },
                    ],
                }
            ))
        );
    }

    #[test]
    fn independent_extension_rejects_after_the_search_bound() {
        let mut selector = selector(extend(false));
        cover(&mut selector, 15);

        assert_eq!(
            selector.selection(),
            Err(BoundarySelectionError::NoCompatibleBoundary)
        );
    }
}
