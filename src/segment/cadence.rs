//! Choosing one close, track-local cadence for every rendition.
//!
//! Pre-roll examines candidates in this order:
//!
//! 1. Observe through the desired duration.
//! 2. Try common video boundaries from latest to earliest below the target.
//! 3. If none is feasible, try later boundaries up to the configured maximum.
//! 4. Snap audio to its encoded grid and replay the complete candidate contract.
//!
//! Video random-access starts must identify the same presentation instant.
//! Audio boundaries stay on their encoded sample grid, including priming.

use std::{cmp::Ordering, num::NonZero, time::Duration};

use crate::{
    domain::{
        DiscoveredTrack, MediaKind, TickDuration, TickTimestamp, Timebase, TrackId, duration_since,
    },
    media::{NormalizedSample, PresentationPlan, PresentedTimingCursor, TimelineCalibration},
};

use super::{CadenceError, SegmentationPolicy, TrackSegmentationPlan, part::AccessUnitCadence};

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
    frame_rate: Option<crate::domain::FrameRate>,
    timestamp_quantum: u64,
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
    tracks: Vec<SelectedBoundary>,
}

#[derive(Clone, Copy)]
enum Target {
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
        policy.validate()?;
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
                frame_rate: match source.parameters {
                    crate::domain::MediaParameters::Video { frame_rate, .. } => frame_rate,
                    _ => None,
                },
                timebase: timing.timebase,
                timestamp_quantum: presentation.timestamp_quantum(source.id, timing.timebase),
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
        // Parts count encoded access units that carry presentable media. Codec
        // pre-skip can shorten the first audible unit without changing that
        // grid. CMAF keeps its encoded start as the first chunk's origin, so
        // using the trimmed duration here would mistake priming for jitter.
        track.access_units.observe(sample.duration());

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

    /// The first timing candidate, primarily useful to inspect boundary discovery.
    /// Admission must replay candidates before committing to one.
    pub fn plan(&self) -> Result<Option<Vec<TrackSegmentationPlan>>, CadenceError> {
        let mut plan = None;
        self.visit_candidates(&mut 0, |tracks, _| {
            plan = Some(tracks);
            Ok::<_, CadenceError>(true)
        })?;
        if plan.is_none() && self.exhausted() {
            return Err(CadenceError::NoSegmentationBoundary);
        }
        Ok(plan)
    }

    /// Enumerates complete plans in preference order without declaring them feasible.
    pub fn visit_candidates<E: From<CadenceError>>(
        &self,
        work: &mut usize,
        mut accept: impl FnMut(Vec<TrackSegmentationPlan>, &mut usize) -> Result<bool, E>,
    ) -> Result<(), E> {
        let videos: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| (track.kind == MediaKind::Video).then_some(index))
            .collect();
        let authority = videos.first().copied().or_else(|| {
            self.tracks
                .iter()
                .position(|track| track.kind == MediaKind::Audio)
        });
        let Some(authority) = authority else {
            return Ok(());
        };
        let required = if videos.is_empty() {
            vec![authority]
        } else {
            videos.clone()
        };
        if !required
            .iter()
            .all(|index| self.has_crossed(*index, self.policy.desired_segment_duration))
        {
            return Ok(());
        }
        let source = &self.tracks[authority];
        let desired = source
            .timebase
            .duration_to_ticks_floor(self.policy.desired_segment_duration);
        let maximum = source
            .timebase
            .duration_to_ticks_floor(self.policy.segment_cap());
        let mut boundaries: Vec<_> = source
            .boundaries
            .iter()
            .filter(|boundary| boundary.start <= maximum)
            .collect();
        // Below the target, prefer the latest boundary; beyond it, extend greedily.
        boundaries.sort_by_key(|boundary| {
            if boundary.start <= desired {
                (false, u64::MAX - boundary.start)
            } else {
                (true, boundary.start)
            }
        });
        for boundary in boundaries {
            // Charge boundary scans as well as replay. Stop on the first
            // accepted candidate instead of materializing every possible plan.
            *work = self.tracks.iter().fold(*work, |work, track| {
                work.saturating_add(track.boundaries.len().saturating_add(1))
            });
            if *work > 1_000_000 {
                return Err(CadenceError::WorkLimitExceeded.into());
            }
            let point = BoundaryPoint {
                track_index: authority,
                ticks: boundary.start,
            };
            let selection = if videos.is_empty() {
                None
            } else {
                let Some(tracks) = self.video_boundaries_covering(&videos, point)? else {
                    continue;
                };
                Some(VideoSelection { tracks })
            };
            let mut tracks = Vec::new();
            for (index, track) in self.tracks.iter().enumerate() {
                match self.plan_track(index, track, Target::Point(point), selection.as_ref()) {
                    Ok(Some(plan)) => tracks.push(plan),
                    Ok(None) | Err(CadenceError::NoSegmentationBoundary) => break,
                    Err(error) => return Err(error.into()),
                }
            }
            if tracks.len() == self.tracks.len() && accept(tracks, work)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Missing evidence may still arrive until every governing track crosses the cap.
    pub fn exhausted(&self) -> bool {
        // A video can lead audio by several batches. Crossing the video cap
        // does not prove that the audio needed by an existing candidate is absent.
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| track.kind != MediaKind::Subtitle)
            .all(|(index, _)| self.has_crossed(index, self.policy.segment_cap()))
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
                if self.compare_points(
                    BoundaryPoint {
                        track_index: *track_index,
                        ticks: boundary.start,
                    },
                    point,
                )? == Ordering::Equal
                {
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
        let part_duration = NonZero::new(
            track.timebase.duration_to_ticks_floor(
                self.policy
                    .desired_part_duration
                    .min(track.timebase.ticks_to_duration(segment_duration.get())),
            ),
        )
        .ok_or(CadenceError::InconsistentPartCadence(track.track_id))?;

        let segment_period = track.frame_rate.and_then(|rate| {
            let numerator =
                u128::from(rate.denominator().get()) * u128::from(track.timebase.den().get());
            let denominator =
                u128::from(rate.numerator().get()) * u128::from(track.timebase.num().get());
            let observed = u128::from(segment_duration.get()) * denominator;
            let frames = (observed + numerator / 2) / numerator;
            let period = frames.checked_mul(numerator)?;
            (frames > 0
                && period.abs_diff(observed) <= denominator * u128::from(track.timestamp_quantum)
                && period != observed)
                .then(|| {
                    Some((
                        u64::try_from(period).ok()?,
                        u64::try_from(denominator).ok()?,
                    ))
                })
                .flatten()
        });
        Ok(Some(TrackSegmentationPlan {
            track_id: track.track_id,
            timebase: track.timebase,
            presentation_origin_pts: track.presentation_origin,
            segmentation_origin_pts: segmentation_origin,
            first_segment_boundary_pts,
            segment_duration,
            segment_period,
            part_duration,
            boundary_tolerance: match track.kind {
                MediaKind::Audio => track.access_units.longest(),
                MediaKind::Video => {
                    if segment_period.is_some() {
                        track.timestamp_quantum
                    } else {
                        0
                    }
                }
                MediaKind::Subtitle => 0,
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
                i128::from(left.ticks)
                    + i128::from(
                        left_track
                            .segmentation_origin
                            .unwrap_or(left_track.presentation_origin),
                    )
                    - i128::from(left_track.presentation_origin),
                right_track.timebase,
                i128::from(right.ticks)
                    + i128::from(
                        right_track
                            .segmentation_origin
                            .unwrap_or(right_track.presentation_origin),
                    )
                    - i128::from(right_track.presentation_origin),
            )
            .ok_or(CadenceError::ComparisonOverflow)
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

    fn policy(maximum_segment_duration: Duration) -> SegmentationPolicy {
        SegmentationPolicy {
            desired_segment_duration: Duration::from_secs(4),
            desired_part_duration: Duration::from_millis(200),
            maximum_segment_duration,
            maximum_part_duration: Duration::from_secs(2),
            early_boundary: Duration::ZERO,
            late_boundary: Duration::ZERO,
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

    fn observer(kinds: &[(u32, MediaKind)], maximum_segment_duration: Duration) -> CadenceObserver {
        let (presentation, timeline) = tracks(kinds);
        CadenceObserver::new(&presentation, &timeline, policy(maximum_segment_duration))
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
    fn overlapping_but_misaligned_video_keyframes_do_not_form_a_boundary() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Video)],
            Duration::from_secs(8),
        );
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 1, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, true);
        observe_video(&mut observer, 1, 4 * VIDEO_SECOND + 1_500, true);
        assert!(observer.plan().expect("search succeeds").is_none());
    }

    #[test]
    fn a_video_track_without_a_compatible_keyframe_extends_the_shared_horizon() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Video)],
            Duration::from_secs(8),
        );
        for track_id in [0, 1] {
            observe_video(&mut observer, track_id, 0, true);
            observe_video(&mut observer, track_id, 4 * VIDEO_SECOND, false);
        }
        assert!(observer.plan().expect("search succeeds").is_none());

        observe_video(&mut observer, 0, 6 * VIDEO_SECOND, true);
        assert!(observer.plan().expect("search succeeds").is_none());
        observe_video(&mut observer, 1, 6 * VIDEO_SECOND, true);

        let plans = observer
            .plan()
            .expect("search succeeds")
            .expect("the first compatible extended cluster resolves immediately");
        assert_eq!(plans[0].segment_duration.get(), 6 * VIDEO_SECOND as u64);
        assert_eq!(plans[1].segment_duration.get(), (6 * VIDEO_SECOND) as u64);
    }

    #[test]
    fn incompatible_video_cadences_are_rejected_at_the_upper_bound() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Video)],
            Duration::from_secs(6),
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
                6 * VIDEO_SECOND + i64::try_from(VIDEO_FRAME).expect("fixture frame fits i64"),
                false,
            );
        }

        assert_eq!(observer.plan(), Err(CadenceError::NoSegmentationBoundary));
    }

    #[test]
    fn strict_search_rejects_once_the_desired_horizon_is_conclusive() {
        let mut observer = observer(&[(0, MediaKind::Video)], Duration::from_secs(4));
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, false);

        assert_eq!(observer.plan(), Err(CadenceError::NoSegmentationBoundary));
    }

    #[test]
    fn video_priority_snaps_audio_to_its_own_access_unit_grid() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Audio)],
            Duration::from_secs(4),
        );
        observe_video(&mut observer, 0, 0, true);
        observe_video(&mut observer, 0, 4 * VIDEO_SECOND, true);
        for frame in 0..=200 {
            observer
                .observe(&audio_sample(
                    1,
                    frame * i64::try_from(AUDIO_FRAME).expect("fixture frame fits i64"),
                    AUDIO_FRAME,
                ))
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
            audio.segmentation_origin_pts
                + i64::try_from(audio.segment_duration.get()).expect("segment duration fits i64")
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
        for initial_padding_samples in [0, 1_024, 2_048, 2_112, 1, 312, 1_023, 3_071] {
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
                policy(Duration::from_secs(4)),
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

            assert_eq!(plan.boundary_tolerance, AUDIO_FRAME);
        }
    }

    #[test]
    fn a_sparse_subtitle_track_never_gates_video() {
        let mut observer = observer(
            &[(0, MediaKind::Video), (1, MediaKind::Subtitle)],
            Duration::from_secs(4),
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
                policy(Duration::from_secs(4)),
            )
            .err(),
            Some(CadenceError::UnknownTrack(TrackId(7)))
        );
        assert_eq!(
            CadenceObserver::new(
                &presentation,
                &calibrated([(0, Timebase::hz90k(), 0), (0, Timebase::hz90k(), 0),]),
                policy(Duration::from_secs(4)),
            )
            .err(),
            Some(CadenceError::DuplicateTrack(TrackId(0)))
        );
    }
}
