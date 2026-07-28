//! Choosing one close, track-local cadence for every rendition.
//!
//! Pre-roll has one greedy rule:
//!
//! 1. Observe through the desired duration.
//! 2. Find the latest keyframe interval shared by every video track.
//! 3. If there is none, keep observing only until the configured extension.
//! 4. Snap audio to the access unit covering the selected video instant.
//!
//! Candidate intervals are compared by elapsed time from each track's first
//! presentable encoded access unit. Their starts may differ by a frame, but an
//! overlap proves the resulting cadences are close. Each track keeps its own
//! start and period, so neither video nor primed audio is forced off its grid.

use std::{cmp::Ordering, num::NonZero, time::Duration};

use crate::{
    domain::{
        DiscoveredTrack, MediaKind, TickDuration, TickTimestamp, Timebase, TrackId, duration_since,
    },
    media::{NormalizedSample, PresentationPlan, PresentedTimingCursor, TimelineCalibration},
};

use super::{
    BoundarySearchPolicy, CadenceError, SegmentationPolicy, TrackSegmentationPlan,
    part::{AccessUnitCadence, select_part_cadence},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Boundary {
    /// Encoded ticks elapsed from this track's segmentation origin.
    start: TickDuration,
    end: TickDuration,
}

/// What pre-roll has learned about one track.
#[derive(Debug)]
struct TrackCadence {
    track_id: TrackId,
    kind: MediaKind,
    timebase: Timebase,
    /// Track-local PTS naming the shared presentation origin.
    presentation_origin: TickTimestamp,
    presented_timing: PresentedTimingCursor,
    /// Encoded start of the first access unit carrying presentable media.
    ///
    /// Keeping the encoded start is essential for audio priming: the first
    /// audible sample may sit in the middle of this access unit, but neither an
    /// MP4 fragment nor a recurring cadence can begin in the middle of one.
    segmentation_origin: Option<TickTimestamp>,
    /// Furthest encoded end observed, relative to `segmentation_origin`.
    observed_until: Option<TickDuration>,
    access_units: AccessUnitCadence,
    /// Random-access units for video; every presentable access unit for audio.
    ///
    /// Subtitle windows are synthetic and therefore need no boundary evidence.
    boundaries: Vec<Boundary>,
}

#[derive(Clone, Copy, Debug)]
struct BoundaryPoint {
    track_index: usize,
    ticks: TickDuration,
}

#[derive(Clone, Copy, Debug)]
struct SelectedBoundary {
    track_index: usize,
    boundary: Boundary,
}

#[derive(Debug)]
struct VideoSelection {
    /// The latest boundary start inside the shared keyframe intersection.
    point: BoundaryPoint,
    tracks: Vec<SelectedBoundary>,
}

enum VideoSearch {
    NoVideo,
    Pending,
    Ready(VideoSelection),
}

#[derive(Clone, Copy)]
enum Target {
    Duration(Duration),
    Point(BoundaryPoint),
}

/// Observes normalized access units until every track can receive a plan.
pub struct CadenceObserver {
    policy: SegmentationPolicy,
    tracks: Vec<TrackCadence>,
}

impl CadenceObserver {
    pub fn new(
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
        policy: SegmentationPolicy,
    ) -> Result<Self, CadenceError> {
        let mut tracks: Vec<TrackCadence> = Vec::with_capacity(timeline.tracks.len());
        for timing in &timeline.tracks {
            if tracks
                .iter()
                .any(|candidate| candidate.track_id == timing.track_id)
            {
                return Err(CadenceError::DuplicateTrack(timing.track_id));
            }
            let source = source_track(presentation, timing.track_id)?;
            tracks.push(TrackCadence {
                track_id: timing.track_id,
                kind: source.kind(),
                timebase: timing.timebase,
                presentation_origin: timing.origin_pts,
                presented_timing: PresentedTimingCursor::for_track(source),
                segmentation_origin: None,
                observed_until: None,
                access_units: AccessUnitCadence::default(),
                boundaries: Vec::new(),
            });
        }

        Ok(Self { policy, tracks })
    }

    pub fn observe(&mut self, sample: &NormalizedSample) -> Result<(), CadenceError> {
        let track_id = sample.track_id();
        // Validated presentations have few tracks; a linear scan avoids a
        // second index and keeps the hot state contiguous.
        let track = self
            .tracks
            .iter_mut()
            .find(|track| track.track_id == track_id)
            .ok_or(CadenceError::UnknownTrack(track_id))?;
        let presented = track
            .presented_timing
            .next(sample)
            .map_err(|source| CadenceError::InvalidSampleTiming { track_id, source })?;
        if presented.duration == 0 {
            // Fully primed audio describes neither the audible origin nor the
            // access-unit cadence used for parts.
            return Ok(());
        }

        let origin = *track.segmentation_origin.get_or_insert(sample.pts());
        let start = duration_since(sample.pts(), origin)
            .ok_or(CadenceError::TimestampOverflow(track_id))?;
        let end = sample
            .pts()
            .checked_add_unsigned(sample.duration())
            .and_then(|end| duration_since(end, origin))
            .ok_or(CadenceError::TimestampOverflow(track_id))?;
        track.observed_until = Some(track.observed_until.map_or(end, |seen| seen.max(end)));
        track.access_units.observe(presented.duration);

        let is_boundary = match track.kind {
            MediaKind::Audio => true,
            MediaKind::Video => sample.random_access(),
            MediaKind::Subtitle => false,
        };
        // Offset zero opens segment zero; selecting it again would create an
        // empty first segment.
        if is_boundary && start > 0 {
            track.boundaries.push(Boundary { start, end });
        }
        Ok(())
    }

    /// Returns a complete plan as soon as the desired horizon is conclusive.
    ///
    /// Extension is considered only when no compatible video cluster exists at
    /// or before the desired duration. As soon as an extended cluster appears,
    /// it wins; pre-roll never waits out unused extension budget.
    pub fn plan(&self) -> Result<Option<Vec<TrackSegmentationPlan>>, CadenceError> {
        let search = self.search_videos()?;
        let target = match &search {
            VideoSearch::Pending => return Ok(None),
            VideoSearch::NoVideo => Target::Duration(self.policy.desired_segment_duration),
            VideoSearch::Ready(selection) => Target::Point(selection.point),
        };

        let selected_videos = match &search {
            VideoSearch::Ready(selection) => Some(selection),
            VideoSearch::NoVideo | VideoSearch::Pending => None,
        };
        let mut plans = Vec::with_capacity(self.tracks.len());
        for (track_index, track) in self.tracks.iter().enumerate() {
            let Some(plan) = self.plan_track(track_index, track, target, selected_videos)? else {
                return Ok(None);
            };
            plans.push(plan);
        }
        Ok(Some(plans))
    }

    fn search_videos(&self) -> Result<VideoSearch, CadenceError> {
        let video_tracks: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| (track.kind == MediaKind::Video).then_some(index))
            .collect();
        if video_tracks.is_empty() {
            return Ok(VideoSearch::NoVideo);
        }

        // Until every video has crossed the inclusive endpoint, a keyframe
        // beginning exactly there could still be the best candidate.
        if !video_tracks
            .iter()
            .all(|index| self.has_crossed(*index, self.policy.desired_segment_duration))
        {
            return Ok(VideoSearch::Pending);
        }
        if let Some(selection) = self.best_common_video_boundary(
            &video_tracks,
            self.policy.desired_segment_duration,
            SearchDirection::LatestAtOrBefore,
        )? {
            return Ok(VideoSearch::Ready(selection));
        }

        let BoundarySearchPolicy::ExtendToNext { maximum_extension } = self.policy.search else {
            return Err(CadenceError::NoSegmentationBoundary);
        };
        let maximum = self
            .policy
            .desired_segment_duration
            .saturating_add(maximum_extension);
        if let Some(selection) = self.best_common_video_boundary(
            &video_tracks,
            maximum,
            SearchDirection::EarliestAfterDesired,
        )? {
            return Ok(VideoSearch::Ready(selection));
        }
        if video_tracks
            .iter()
            .all(|index| self.has_crossed(*index, maximum))
        {
            return Err(CadenceError::NoSegmentationBoundary);
        }
        Ok(VideoSearch::Pending)
    }

    /// Finds one overlapping keyframe interval across all video tracks.
    ///
    /// The overlap is the closeness rule. It accepts starts that differ by up
    /// to a frame while rejecting periods that would visibly drift apart.
    fn best_common_video_boundary(
        &self,
        video_tracks: &[usize],
        limit: Duration,
        direction: SearchDirection,
    ) -> Result<Option<VideoSelection>, CadenceError> {
        let mut selected: Option<VideoSelection> = None;
        for track_index in video_tracks {
            for boundary in &self.tracks[*track_index].boundaries {
                let point = BoundaryPoint {
                    track_index: *track_index,
                    ticks: boundary.start,
                };
                if !direction.admits(
                    boundary.start,
                    self.tracks[*track_index].timebase,
                    self.policy.desired_segment_duration,
                    limit,
                ) {
                    continue;
                }
                let Some(tracks) = self.video_boundaries_covering(video_tracks, point)? else {
                    continue;
                };
                let replace = match &selected {
                    None => true,
                    Some(current) => {
                        self.compare_points(point, current.point)? == direction.preference()
                    }
                };
                if replace {
                    selected = Some(VideoSelection { point, tracks });
                }
            }
        }
        Ok(selected)
    }

    fn video_boundaries_covering(
        &self,
        video_tracks: &[usize],
        point: BoundaryPoint,
    ) -> Result<Option<Vec<SelectedBoundary>>, CadenceError> {
        let mut selected = Vec::with_capacity(video_tracks.len());
        for track_index in video_tracks {
            let mut covering = None;
            for boundary in &self.tracks[*track_index].boundaries {
                if self.boundary_covers_point(*track_index, *boundary, point)? {
                    covering = Some(*boundary);
                    break;
                }
            }
            let Some(boundary) = covering else {
                return Ok(None);
            };
            selected.push(SelectedBoundary {
                track_index: *track_index,
                boundary,
            });
        }
        Ok(Some(selected))
    }

    fn plan_track(
        &self,
        track_index: usize,
        track: &TrackCadence,
        target: Target,
        videos: Option<&VideoSelection>,
    ) -> Result<Option<TrackSegmentationPlan>, CadenceError> {
        let (segmentation_origin, segment_duration) = match track.kind {
            MediaKind::Video => {
                let Some(selection) = videos else {
                    return Err(CadenceError::NoSegmentationBoundary);
                };
                let boundary = selection
                    .tracks
                    .iter()
                    .find(|selection| selection.track_index == track_index)
                    .map(|selection| selection.boundary)
                    .ok_or(CadenceError::NoSegmentationBoundary)?;
                let Some(origin) = track.segmentation_origin else {
                    return Ok(None);
                };
                (origin, boundary.start)
            }
            MediaKind::Audio => {
                let Some(origin) = track.segmentation_origin else {
                    return Ok(None);
                };
                let Some(boundary) = self.boundary_covering_target(track_index, target)? else {
                    if self.target_is_observed(track_index, target)? {
                        return Err(CadenceError::NoSegmentationBoundary);
                    }
                    return Ok(None);
                };
                (origin, boundary.start)
            }
            MediaKind::Subtitle => {
                let duration = self.target_ticks(track_index, target)?;
                (track.presentation_origin, duration)
            }
        };
        let segment_duration =
            NonZero::new(segment_duration).ok_or(CadenceError::NoSegmentationBoundary)?;
        let first_segment_boundary_pts = segmentation_origin
            .checked_add_unsigned(segment_duration.get())
            .ok_or(CadenceError::TimestampOverflow(track.track_id))?;
        let part = select_part_cadence(
            track.track_id,
            track.kind,
            track.access_units,
            track
                .timebase
                .duration_to_ticks(self.policy.desired_part_duration),
            segment_duration.get(),
        )?;

        Ok(Some(TrackSegmentationPlan {
            track_id: track.track_id,
            timebase: track.timebase,
            presentation_origin_pts: track.presentation_origin,
            segmentation_origin_pts: segmentation_origin,
            first_segment_boundary_pts,
            segment_duration,
            part_access_units: part.access_units,
            part_duration: part.duration,
            boundary_tolerance: match track.kind {
                MediaKind::Audio => track.access_units.longest(),
                MediaKind::Subtitle | MediaKind::Video => 0,
            },
        }))
    }

    fn boundary_covering_target(
        &self,
        track_index: usize,
        target: Target,
    ) -> Result<Option<Boundary>, CadenceError> {
        for boundary in &self.tracks[track_index].boundaries {
            let covers = match target {
                Target::Duration(duration) => {
                    let ticks = self.tracks[track_index]
                        .timebase
                        .duration_to_ticks_floor(duration);
                    boundary.start <= ticks && ticks < boundary.end
                }
                Target::Point(point) => {
                    self.boundary_covers_point(track_index, *boundary, point)?
                }
            };
            if covers {
                return Ok(Some(*boundary));
            }
        }
        Ok(None)
    }

    fn boundary_covers_point(
        &self,
        track_index: usize,
        boundary: Boundary,
        point: BoundaryPoint,
    ) -> Result<bool, CadenceError> {
        let start = BoundaryPoint {
            track_index,
            ticks: boundary.start,
        };
        let end = BoundaryPoint {
            track_index,
            ticks: boundary.end,
        };
        Ok(self.compare_points(start, point)? != Ordering::Greater
            && self.compare_points(end, point)? == Ordering::Greater)
    }

    fn target_is_observed(&self, track_index: usize, target: Target) -> Result<bool, CadenceError> {
        let track = &self.tracks[track_index];
        let Some(observed) = track.observed_until else {
            return Ok(false);
        };
        match target {
            Target::Duration(duration) => {
                Ok(observed > track.timebase.duration_to_ticks_ceil(duration))
            }
            Target::Point(point) => self
                .compare_points(
                    BoundaryPoint {
                        track_index,
                        ticks: observed,
                    },
                    point,
                )
                .map(|ordering| ordering == Ordering::Greater),
        }
    }

    fn target_ticks(
        &self,
        track_index: usize,
        target: Target,
    ) -> Result<TickDuration, CadenceError> {
        let track = &self.tracks[track_index];
        match target {
            Target::Duration(duration) => Ok(track.timebase.duration_to_ticks(duration)),
            Target::Point(point) => {
                let source = &self.tracks[point.track_index];
                let ticks = TickTimestamp::try_from(point.ticks)
                    .ok()
                    .and_then(|ticks| source.timebase.checked_rescale_ticks(ticks, track.timebase))
                    .and_then(|ticks| TickDuration::try_from(ticks).ok())
                    .ok_or(CadenceError::HorizonOverflow(track.track_id))?;
                Ok(ticks)
            }
        }
    }

    fn has_crossed(&self, track_index: usize, duration: Duration) -> bool {
        let track = &self.tracks[track_index];
        track
            .observed_until
            .is_some_and(|observed| observed > track.timebase.duration_to_ticks_ceil(duration))
    }

    fn compare_points(
        &self,
        left: BoundaryPoint,
        right: BoundaryPoint,
    ) -> Result<Ordering, CadenceError> {
        let left_track = &self.tracks[left.track_index];
        let right_track = &self.tracks[right.track_index];
        left_track
            .timebase
            .compare_offsets(
                i128::from(left.ticks),
                right_track.timebase,
                i128::from(right.ticks),
            )
            .ok_or(CadenceError::ComparisonOverflow)
    }
}

#[derive(Clone, Copy)]
enum SearchDirection {
    LatestAtOrBefore,
    EarliestAfterDesired,
}

impl SearchDirection {
    fn admits(
        self,
        ticks: TickDuration,
        timebase: Timebase,
        desired: Duration,
        limit: Duration,
    ) -> bool {
        let desired = timebase.duration_to_ticks_floor(desired);
        let limit = timebase.duration_to_ticks_floor(limit);
        match self {
            Self::LatestAtOrBefore => ticks <= desired,
            Self::EarliestAfterDesired => desired < ticks && ticks <= limit,
        }
    }

    fn preference(self) -> Ordering {
        match self {
            Self::LatestAtOrBefore => Ordering::Greater,
            Self::EarliestAfterDesired => Ordering::Less,
        }
    }
}

fn source_track(
    presentation: &PresentationPlan,
    track_id: TrackId,
) -> Result<&DiscoveredTrack, CadenceError> {
    presentation
        .catalog()
        .get(track_id)
        .ok_or(CadenceError::UnknownTrack(track_id))
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{
            AudioTrim, Codec, MediaKind, MediaParameters, Payload, Timebase, fixtures::TrackBuilder,
        },
        media::{
            AudioSample, VideoSample,
            fixtures::{audio_sample, calibrated, presentation},
        },
    };

    use super::*;

    const VIDEO_SECOND: i64 = 90_000;
    const VIDEO_FRAME: u64 = 3_000;
    const AUDIO_FRAME: u64 = 1_024;

    fn audio_timebase() -> Timebase {
        Timebase::new(nz::u32!(1), nz::u32!(48_000))
    }

    fn policy(search: BoundarySearchPolicy) -> SegmentationPolicy {
        SegmentationPolicy {
            desired_segment_duration: Duration::from_secs(4),
            desired_part_duration: Duration::from_millis(200),
            search,
        }
    }

    fn tracks(kinds: &[(u32, MediaKind)]) -> (PresentationPlan, TimelineCalibration) {
        let presentation = presentation(
            kinds
                .iter()
                .map(|(id, kind)| {
                    let timebase = match kind {
                        MediaKind::Audio => audio_timebase(),
                        MediaKind::Subtitle | MediaKind::Video => Timebase::hz90k(),
                    };
                    TrackBuilder::new(*id, *kind)
                        .timebase(timebase)
                        .first_pts(Some(0))
                        .build()
                })
                .collect(),
        );
        let timeline = calibrated(kinds.iter().map(|(id, kind)| {
            let timebase = match kind {
                MediaKind::Audio => audio_timebase(),
                MediaKind::Subtitle | MediaKind::Video => Timebase::hz90k(),
            };
            (*id, timebase, 0)
        }));
        (presentation, timeline)
    }

    fn observer(kinds: &[(u32, MediaKind)], search: BoundarySearchPolicy) -> CadenceObserver {
        let (presentation, timeline) = tracks(kinds);
        CadenceObserver::new(&presentation, &timeline, policy(search))
            .expect("test observer is valid")
    }

    fn video(track_id: u32, pts: i64, duration: u64, random_access: bool) -> NormalizedSample {
        NormalizedSample::Video(VideoSample {
            track_id: TrackId(track_id),
            codec: Codec::H264,
            pts,
            dts: pts,
            duration,
            random_access,
            payload: Payload::default(),
        })
    }

    fn observe_video(observer: &mut CadenceObserver, track_id: u32, pts: i64, random_access: bool) {
        observer
            .observe(&video(track_id, pts, VIDEO_FRAME, random_access))
            .expect("video is observable");
    }

    #[test]
    fn every_video_track_constrains_the_plan_without_requiring_equal_ticks() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Video)],
            BoundarySearchPolicy::ExtendToNext {
                maximum_extension: Duration::from_secs(4),
            },
        );
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 1, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, true);
        // The starts differ, but the two frame intervals overlap.
        observe_video(&mut observer, 1, 4 * VIDEO_SECOND + 1_500, true);

        let plans = observer
            .plan()
            .expect("search succeeds")
            .expect("both tracks supplied a compatible boundary");

        assert_eq!(plans[0].segment_duration.get(), 4 * VIDEO_SECOND as u64);
        assert_eq!(
            plans[1].segment_duration.get(),
            (4 * VIDEO_SECOND + 1_500) as u64
        );
    }

    #[test]
    fn a_video_track_without_a_compatible_keyframe_extends_the_shared_horizon() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Video)],
            BoundarySearchPolicy::ExtendToNext {
                maximum_extension: Duration::from_secs(4),
            },
        );
        for track_id in [0, 1] {
            observe_video(&mut observer, track_id, 0, true);
            observe_video(&mut observer, track_id, 4 * VIDEO_SECOND, false);
        }
        assert!(observer.plan().expect("search succeeds").is_none());

        observe_video(&mut observer, 0, 6 * VIDEO_SECOND, true);
        assert!(observer.plan().expect("search succeeds").is_none());
        observe_video(&mut observer, 1, 6 * VIDEO_SECOND + 1_000, true);

        let plans = observer
            .plan()
            .expect("search succeeds")
            .expect("the first compatible extended cluster resolves immediately");
        assert_eq!(plans[0].segment_duration.get(), 6 * VIDEO_SECOND as u64);
        assert_eq!(
            plans[1].segment_duration.get(),
            (6 * VIDEO_SECOND + 1_000) as u64
        );
    }

    #[test]
    fn incompatible_video_cadences_are_rejected_at_the_upper_bound() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Video)],
            BoundarySearchPolicy::ExtendToNext {
                maximum_extension: Duration::from_secs(2),
            },
        );
        for track_id in [0, 1] {
            observe_video(&mut observer, track_id, 0, true);
        }
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, true);
        observe_video(&mut observer, 1, 3 * VIDEO_SECOND, true);
        for track_id in [0, 1] {
            observe_video(
                &mut observer,
                track_id,
                6 * VIDEO_SECOND + VIDEO_FRAME as i64,
                false,
            );
        }

        assert_eq!(observer.plan(), Err(CadenceError::NoSegmentationBoundary));
    }

    #[test]
    fn strict_search_rejects_once_the_desired_horizon_is_conclusive() {
        let mut observer = observer(
            &[(0, MediaKind::Video)],
            BoundarySearchPolicy::AtOrBeforeDesired,
        );
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, false);

        assert_eq!(observer.plan(), Err(CadenceError::NoSegmentationBoundary));
    }

    #[test]
    fn video_priority_snaps_audio_to_its_own_access_unit_grid() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Audio)],
            BoundarySearchPolicy::AtOrBeforeDesired,
        );
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, true);
        for frame in 0..=200 {
            observer
                .observe(&audio_sample(1, frame * AUDIO_FRAME as i64, AUDIO_FRAME))
                .expect("audio is observable");
        }

        let plans = observer
            .plan()
            .expect("search succeeds")
            .expect("video and audio are ready");
        let audio = &plans[1];

        assert_eq!(audio.segment_duration.get() % AUDIO_FRAME, 0);
        assert_eq!(
            audio.first_segment_boundary_pts,
            audio.segmentation_origin_pts + audio.segment_duration.get() as i64
        );
        assert!(
            Duration::from_secs(4).abs_diff(
                audio
                    .timebase
                    .ticks_to_duration(audio.segment_duration.get())
            ) <= audio.timebase.ticks_to_duration(AUDIO_FRAME)
        );
    }

    #[test]
    fn priming_never_moves_audio_off_the_encoded_grid() {
        for initial_padding_samples in [0, 1_024, 2_048, 2_112, 1] {
            let track = TrackBuilder::new(0, MediaKind::Audio)
                .codec(Codec::Aac)
                .timebase(audio_timebase())
                .first_pts(Some(-i64::from(initial_padding_samples)))
                .parameters(MediaParameters::Audio {
                    sample_rate: nz::u32!(48_000),
                    channels: nz::u16!(2),
                    frame_size: Some(nz::u32!(1_024)),
                    bit_depth: None,
                    timing: crate::domain::AudioTiming {
                        initial_padding_samples,
                        ..crate::domain::AudioTiming::default()
                    },
                })
                .build();
            let first_pts = -i64::from(initial_padding_samples);
            let mut observer = CadenceObserver::new(
                &presentation(vec![track]),
                &calibrated([(0, audio_timebase(), first_pts)]),
                policy(BoundarySearchPolicy::AtOrBeforeDesired),
            )
            .expect("test observer is valid");
            for frame in 0..=240_u64 {
                observer
                    .observe(&NormalizedSample::Audio(AudioSample {
                        track_id: TrackId(0),
                        codec: Codec::Aac,
                        pts: first_pts + i64::try_from(frame * AUDIO_FRAME).expect("PTS fits"),
                        duration: AUDIO_FRAME,
                        trim: AudioTrim {
                            leading_samples: if frame == 0 {
                                initial_padding_samples
                            } else {
                                0
                            },
                            trailing_samples: 0,
                        },
                        payload: Payload::default(),
                    }))
                    .expect("primed audio is observable");
            }

            let plan = observer
                .plan()
                .expect("search succeeds")
                .expect("audio supplied the desired horizon")
                .remove(0);
            assert_eq!((plan.segmentation_origin_pts - first_pts) % 1_024, 0);
            assert_eq!(plan.segment_duration.get() % AUDIO_FRAME, 0);
            assert_eq!((plan.first_segment_boundary_pts - first_pts) % 1_024, 0);
        }
    }

    #[test]
    fn a_sparse_subtitle_track_never_gates_video() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Subtitle)],
            BoundarySearchPolicy::AtOrBeforeDesired,
        );
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, true);

        let plans = observer
            .plan()
            .expect("search succeeds")
            .expect("subtitles do not gate");
        let subtitle = &plans[1];
        assert_eq!(subtitle.segmentation_origin_pts, 0);
        assert_eq!(subtitle.segment_duration.get(), 4 * VIDEO_SECOND as u64);
    }

    #[test]
    fn unknown_and_duplicate_tracks_are_rejected() {
        let (presentation, _) = tracks(&[(0, MediaKind::Video)]);
        assert_eq!(
            CadenceObserver::new(
                &presentation,
                &calibrated([(7, Timebase::hz90k(), 0)]),
                policy(BoundarySearchPolicy::AtOrBeforeDesired),
            )
            .err(),
            Some(CadenceError::UnknownTrack(TrackId(7)))
        );
        assert_eq!(
            CadenceObserver::new(
                &presentation,
                &calibrated([(0, Timebase::hz90k(), 0), (0, Timebase::hz90k(), 0),]),
                policy(BoundarySearchPolicy::AtOrBeforeDesired),
            )
            .err(),
            Some(CadenceError::DuplicateTrack(TrackId(0)))
        );
    }
}
