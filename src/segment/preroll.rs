use std::num::NonZero;

use tokio::time::{Instant, timeout};

use crate::{
    domain::{TickDuration, TickTimestamp, TrackId, duration_since},
    media::{
        NormalizedSample, PresentationPlan, PresentedTimingCursor, Rounding, SampleSource,
        TimelineCalibration, TrackTimeline,
    },
    observe::{EventSink, SessionEvent},
    source::InputState,
};

use super::{
    BoundarySelection, BoundarySelectionError, BoundarySelectionStatus, BoundarySelector,
    PrerollError, PrerollLimits, SegmentationPlan, SegmentationPolicy, TrackSegmentationPlan,
    boundary::select_part_duration,
};

/// Enough for a second or two of multi-track media, so the common case reaches
/// steady state within the first batch or two and never reallocates after.
const INITIAL_BUFFER_SAMPLES: usize = 256;

pub struct PrerollRequest<'a> {
    pub presentation: &'a PresentationPlan,
    pub timeline: &'a TimelineCalibration,
    pub limits: PrerollLimits,
    pub policy: SegmentationPolicy,
}

#[derive(Debug)]
pub struct Preroll {
    pub segmentation: SegmentationPlan,
    /// Terminal state observed while planning, if the locking batch was also
    /// the source's last. Carried forward so the live loop never has to poll an
    /// exhausted transport merely to rediscover why it ended.
    pub input_state: InputState,
    /// Samples consumed while planning.
    ///
    /// Replayed into the live tail once it exists, so boundary discovery costs
    /// startup latency but never media.
    pub buffered: Vec<NormalizedSample>,
}

/// Observes media until segmentation cadence can be fixed for the session.
///
/// Borrows the sample supply rather than owning it. Everything pre-roll needs
/// is already running by the time it is called, and everything it produces is
/// data, so there is no reason for the source or the normalizer to make a round
/// trip through this function.
pub async fn run(
    source: &mut dyn SampleSource,
    request: PrerollRequest<'_>,
    events: &EventSink,
) -> Result<Preroll, PrerollError> {
    let PrerollRequest {
        presentation,
        timeline,
        limits,
        policy,
    } = request;

    let mut selector = BoundarySelector::new(presentation, timeline, policy)?;
    let horizons = media_horizons(timeline, limits)?;
    let mut buffered = Vec::with_capacity(INITIAL_BUFFER_SAMPLES);
    let mut buffered_bytes = 0_usize;
    let deadline = Instant::now() + limits.maximum_wall_time;

    loop {
        if let Some(selection) = ready_selection(&selector)? {
            return lock(
                presentation,
                timeline,
                policy,
                selection,
                buffered,
                InputState::Open,
                events,
            );
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PrerollError::LimitExceeded);
        }

        // Append straight into the retained buffer: the samples pre-roll
        // inspects are exactly the ones the live tail will replay.
        let observed_from = buffered.len();
        let state = timeout(remaining, source.next_batch(&mut buffered))
            .await
            .map_err(|_| PrerollError::LimitExceeded)??;

        // The buffer was lent out as an `Appender`, so it can only have grown
        // and this range always exists. That is the whole payoff of the narrower
        // type: no length check, and nothing to do if one failed.
        let retained = buffered.len();
        for sample in &buffered[observed_from..] {
            admit(
                &mut selector,
                &horizons,
                &mut buffered_bytes,
                retained,
                limits,
                sample,
            )?;
        }

        if !state.is_open() {
            return match ready_selection(&selector)? {
                Some(selection) => lock(
                    presentation,
                    timeline,
                    policy,
                    selection,
                    buffered,
                    state,
                    events,
                ),
                None => Err(PrerollError::NoSegmentationBoundary),
            };
        }
    }
}

/// The furthest presentation time a track may reach before pre-roll gives up.
#[derive(Clone, Copy, Debug)]
struct MediaHorizon {
    track_id: TrackId,
    maximum_pts: TickTimestamp,
}

fn media_horizons(
    timeline: &TimelineCalibration,
    limits: PrerollLimits,
) -> Result<Vec<MediaHorizon>, PrerollError> {
    timeline
        .tracks
        .iter()
        .map(|track| {
            // Rounded up: a track that reaches exactly the budget has not
            // exceeded it, and rejecting it there would fail publishers whose
            // cadence merely lands on the boundary.
            let maximum_pts = track
                .horizon(limits.maximum_media_duration, Rounding::Up)
                .ok_or(BoundarySelectionError::HorizonOverflow(track.track_id))?;
            Ok(MediaHorizon {
                track_id: track.track_id,
                maximum_pts,
            })
        })
        .collect()
}

/// Accepts one buffered sample, or rejects the session for exceeding a bound.
///
/// `retained` is how many samples the buffer now holds in total, not how many
/// this batch contributed: the budget is on what pre-roll is keeping, and it is
/// already keeping every sample it has looked at.
fn admit(
    selector: &mut BoundarySelector,
    horizons: &[MediaHorizon],
    buffered_bytes: &mut usize,
    retained: usize,
    limits: PrerollLimits,
    sample: &NormalizedSample,
) -> Result<(), PrerollError> {
    if retained > limits.maximum_buffered_samples {
        return Err(PrerollError::LimitExceeded);
    }

    let track_id = sample.track_id();
    let horizon = horizons
        .iter()
        .find(|horizon| horizon.track_id == track_id)
        .ok_or(BoundarySelectionError::UnknownTrack(track_id))?;
    let end = sample
        .pts()
        .checked_add_unsigned(sample.duration())
        .ok_or(BoundarySelectionError::TimestampOverflow(track_id))?;
    if end > horizon.maximum_pts {
        return Err(PrerollError::LimitExceeded);
    }

    // Charged at full retained cost rather than payload size. An input of empty
    // access units would otherwise buffer forever: it advances neither the byte
    // budget nor, if its durations are zero, the media horizon.
    let total = buffered_bytes
        .checked_add(sample.retained_bytes())
        .ok_or(PrerollError::LimitExceeded)?;
    if total > limits.maximum_buffered_bytes {
        return Err(PrerollError::LimitExceeded);
    }

    selector.observe(sample)?;
    *buffered_bytes = total;
    Ok(())
}

fn ready_selection(selector: &BoundarySelector) -> Result<Option<BoundarySelection>, PrerollError> {
    match selector.selection()? {
        BoundarySelectionStatus::Pending => Ok(None),
        BoundarySelectionStatus::Ready(selection) => Ok(Some(selection)),
    }
}

fn lock(
    presentation: &PresentationPlan,
    timeline: &TimelineCalibration,
    policy: SegmentationPolicy,
    selection: BoundarySelection,
    buffered: Vec<NormalizedSample>,
    input_state: InputState,
    events: &EventSink,
) -> Result<Preroll, PrerollError> {
    let mut tracks = Vec::with_capacity(selection.tracks().len());
    for boundary in selection.tracks() {
        let timing = timeline
            .get(boundary.track_id)
            .ok_or(BoundarySelectionError::UnknownTrack(boundary.track_id))?;
        let source = presentation
            .catalog()
            .get(boundary.track_id)
            .ok_or(BoundarySelectionError::UnknownTrack(boundary.track_id))?;
        tracks.push(plan_track(&buffered, policy, timing, source, boundary.pts)?);
    }

    let segmentation = SegmentationPlan::new(presentation, tracks)?;
    events.emit(SessionEvent::SegmentationLocked {
        segment: segmentation.longest_segment_duration(),
        part: segmentation.shortest_part_duration(),
        aligned: policy.is_aligned(),
    });

    Ok(Preroll {
        segmentation,
        input_state,
        buffered,
    })
}

fn plan_track(
    buffered: &[NormalizedSample],
    policy: SegmentationPolicy,
    timing: &TrackTimeline,
    source: &crate::domain::DiscoveredTrack,
    boundary_pts: TickTimestamp,
) -> Result<TrackSegmentationPlan, PrerollError> {
    let segmentation_origin_pts = segmentation_origin(buffered, timing, source)?;
    let segment_duration = duration_since(boundary_pts, segmentation_origin_pts)
        .and_then(NonZero::<TickDuration>::new)
        .ok_or(PrerollError::NoSegmentationBoundary)?;
    let part_duration = select_part_duration(
        buffered,
        source,
        segmentation_origin_pts,
        boundary_pts,
        timing
            .timebase
            .duration_to_ticks(policy.desired_part_duration),
    )
    .map_err(|source| BoundarySelectionError::InvalidSampleTiming {
        track_id: timing.track_id,
        source,
    })?
    .ok_or(PrerollError::NoPartDuration(timing.track_id))?;

    Ok(TrackSegmentationPlan {
        track_id: timing.track_id,
        timebase: timing.timebase,
        presentation_origin_pts: timing.origin_pts,
        segmentation_origin_pts,
        first_segment_boundary_pts: boundary_pts,
        segment_duration,
        part_duration,
    })
}

/// Finds the access unit that first carries audible media at or after the
/// shared origin, without changing the origin used to rebase container
/// timestamps.
///
/// # Why this is the access unit's start, not the audible instant
///
/// [`TrackSegmentationPlan::segment_duration`] is the distance from here to the
/// first boundary, and the muxer reuses it as a repeating period. Boundaries
/// can only fall on access-unit starts — a fragment cannot be cut mid-unit — so
/// the period has to be a whole number of them, which it is only if this point
/// is on the grid too. Otherwise the residue accumulates and boundary two lands
/// between units.
///
/// Codec priming is what makes the distinction bite. It is not generally a
/// whole number of access units: a 2112-sample encoder delay against
/// 1024-sample AAC frames leaves the first audible sample 64 ticks inside its
/// unit. Taking the audible instant would put the origin — and therefore every
/// later boundary — off the grid. Taking the unit's start keeps segmentation
/// aligned, and the 64 ticks of priming still inside it are suppressed by the
/// edit list the muxer emits, which is where that belongs.
fn segmentation_origin(
    buffered: &[NormalizedSample],
    timing: &TrackTimeline,
    source: &crate::domain::DiscoveredTrack,
) -> Result<TickTimestamp, PrerollError> {
    let mut origin = None;
    let mut presented_timing = PresentedTimingCursor::for_track(source);
    for sample in buffered
        .iter()
        .filter(|sample| sample.track_id() == timing.track_id)
    {
        let presented = presented_timing.next(sample).map_err(|source| {
            BoundarySelectionError::InvalidSampleTiming {
                track_id: timing.track_id,
                source,
            }
        })?;
        let end = presented
            .end()
            .ok_or(BoundarySelectionError::TimestampOverflow(timing.track_id))?;
        if presented.duration > 0 && end > timing.origin_pts {
            // The unit's own start, so the grid is preserved. Clamping to the
            // shared origin would reintroduce the offset for a unit that
            // straddles it, so a straddling unit keeps its start and the
            // segment simply begins fractionally before the origin.
            let candidate = sample.pts();
            origin =
                Some(origin.map_or(candidate, |current: TickTimestamp| current.min(candidate)));
        }
    }
    origin.ok_or_else(|| BoundarySelectionError::UnknownTrack(timing.track_id).into())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc, time::Duration};

    use parking_lot::Mutex;

    use crate::{
        domain::{Appender, BoxFuture, SessionId},
        media::{
            MediaError,
            fixtures::{
                VIDEO_SECOND, video_presentation as presentation, video_timeline as timeline,
            },
        },
        observe::{EventObserver, Events, SessionEvent},
    };

    use super::*;

    /// An AAC track whose first packet carries FFmpeg's complete skip count.
    fn primed_aac(
        initial_padding_samples: u32,
    ) -> (crate::domain::DiscoveredTrack, Vec<NormalizedSample>) {
        use crate::domain::{
            AudioTiming, AudioTrim, Codec, MediaKind, MediaParameters, Payload, Timebase,
            fixtures::TrackBuilder,
        };
        use crate::media::AudioSample;

        const FRAME: u64 = 1_024;
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let track = TrackBuilder::new(0, MediaKind::Audio)
            .codec(Codec::Aac)
            .timebase(timebase)
            .parameters(MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(2),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: AudioTiming {
                    initial_padding_samples,
                    ..AudioTiming::default()
                },
            })
            .build();
        let first_pts = -i64::from(initial_padding_samples);
        let samples = (0..32_u64)
            .map(|index| {
                NormalizedSample::Audio(AudioSample {
                    track_id: TrackId(0),
                    codec: Codec::Aac,
                    pts: first_pts + i64::try_from(index * FRAME).expect("fixture PTS fits"),
                    duration: FRAME,
                    trim: AudioTrim {
                        leading_samples: if index == 0 {
                            initial_padding_samples
                        } else {
                            0
                        },
                        trailing_samples: 0,
                    },
                    payload: Payload::default(),
                })
            })
            .collect();
        (track, samples)
    }

    #[test]
    fn the_segmentation_origin_stays_on_the_access_unit_grid_through_priming() {
        // 2112 samples is the Apple/iTunes AAC encoder delay: two whole
        // 1024-sample frames plus 64. The audible timeline therefore starts
        // mid-frame, but segment boundaries can only fall on frame starts, so
        // the segmentation origin must be the frame — not the audible instant.
        for initial_padding_samples in [0, 1_024, 2_048, 2_112, 1] {
            let (track, samples) = primed_aac(initial_padding_samples);
            let timing = TrackTimeline {
                track_id: TrackId(0),
                timebase: track.timebase,
                origin_pts: 0,
            };
            let first_pts = -i64::from(initial_padding_samples);

            let origin = segmentation_origin(&samples, &timing, &track)
                .expect("a primed track still has an audible origin");

            assert_eq!(
                (origin - first_pts) % 1_024,
                0,
                "priming of {initial_padding_samples} samples pushed the \
                 segmentation origin to {origin}, off the access-unit grid"
            );
            // A boundary is always a real sample PTS, so a grid-aligned origin
            // is exactly what makes the period a whole number of frames — which
            // is what stops boundary two landing between access units.
            let boundary = first_pts + 20 * 1_024;
            let period = boundary - origin;
            assert_eq!(
                period % 1_024,
                0,
                "period {period} is not a whole number of access units"
            );

            let track_plan = plan_track(&samples, policy(), &timing, &track, boundary)
                .expect("primed audio produces a complete track plan");
            let presentation = crate::media::fixtures::presentation(vec![track]);
            SegmentationPlan::new(&presentation, vec![track_plan])
                .expect("a straddling access unit is valid plan timing");
        }
    }

    /// Replays prepared batches, then reports the input as exhausted.
    struct Replay {
        batches: VecDeque<Vec<NormalizedSample>>,
    }

    impl Replay {
        fn new(batches: Vec<Vec<NormalizedSample>>) -> Self {
            Self {
                batches: batches.into(),
            }
        }
    }

    impl SampleSource for Replay {
        fn next_batch<'a>(
            &'a mut self,
            out: &'a mut dyn Appender<NormalizedSample>,
        ) -> BoxFuture<'a, Result<InputState, MediaError>> {
            Box::pin(async move {
                match self.batches.pop_front() {
                    Some(batch) => {
                        for sample in batch {
                            out.push(sample);
                        }
                        Ok(if self.batches.is_empty() {
                            InputState::Closed
                        } else {
                            InputState::Open
                        })
                    }
                    None => Ok(InputState::Closed),
                }
            })
        }
    }

    struct Stalled;

    impl SampleSource for Stalled {
        fn next_batch<'a>(
            &'a mut self,
            _out: &'a mut dyn Appender<NormalizedSample>,
        ) -> BoxFuture<'a, Result<InputState, MediaError>> {
            Box::pin(std::future::pending())
        }
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<SessionEvent>>,
    }

    impl EventObserver for Recorder {
        fn observe(&self, _session: SessionId, event: SessionEvent) {
            self.events.lock().push(event);
        }
    }

    #[test]
    fn priming_does_not_shift_segment_or_part_cadence() {
        let timebase = crate::domain::Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let source = crate::domain::fixtures::TrackBuilder::new(0, crate::domain::MediaKind::Audio)
            .timebase(timebase)
            .parameters(crate::domain::MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(2),
                frame_size: Some(nz::u32!(1_024)),
                bit_depth: None,
                timing: crate::domain::AudioTiming {
                    initial_padding_samples: 1_024,
                    ..crate::domain::AudioTiming::default()
                },
            })
            .build();
        let timing = crate::media::TrackTimeline {
            track_id: TrackId(0),
            timebase,
            origin_pts: 0,
        };
        let mut buffered = vec![NormalizedSample::Audio(crate::media::AudioSample {
            track_id: TrackId(0),
            codec: crate::domain::Codec::Aac,
            pts: -1_024,
            duration: 1_024,
            trim: crate::domain::AudioTrim {
                leading_samples: 1_024,
                trailing_samples: 0,
            },
            payload: crate::domain::Payload::default(),
        })];
        buffered.extend(
            (0..20).map(|frame| crate::media::fixtures::audio_sample(0, frame * 1_024, 1_024)),
        );

        let plan = plan_track(&buffered, policy(), &timing, &source, 20 * 1_024)
            .expect("primed audio produces a plan");

        assert_eq!(plan.presentation_origin_pts, 0);
        assert_eq!(plan.segmentation_origin_pts, 0);
        assert_eq!(plan.first_segment_boundary_pts, 20 * 1_024);
        assert_eq!(plan.segment_duration.get(), 20 * 1_024);
        assert_eq!(
            plan.part_duration.get(),
            9 * 1_024,
            "the 200 ms preference is evaluated on audible AAC access units"
        );
    }

    #[tokio::test]
    async fn presentation_origin_remains_calibrated_when_media_starts_later() {
        // A calibrated origin can fall between access units. It remains the
        // timestamp-rebasing anchor; only segment accounting advances to the
        // first effective access unit.
        let calibrated = crate::media::fixtures::calibrated([(
            0,
            crate::domain::Timebase::hz90k(),
            VIDEO_SECOND / 2,
        )]);
        let mut source = Replay::new(vec![
            (1..24)
                .map(|second| sample(second, second % 4 == 0, 1))
                .collect(),
        ]);

        let locked = run(
            &mut source,
            PrerollRequest {
                presentation: &presentation(),
                timeline: &calibrated,
                limits: limits(64, Duration::from_secs(30)),
                policy: policy(),
            },
            &sink(),
        )
        .await
        .expect("segmentation locks");

        let track = locked
            .segmentation
            .get(TrackId(0))
            .expect("the plan covers the track");
        assert_eq!(
            track.presentation_origin_pts,
            VIDEO_SECOND / 2,
            "calibration is not snapped onto an access-unit boundary"
        );
        assert_eq!(
            track.segmentation_origin_pts, VIDEO_SECOND,
            "segment accounting starts at the first effective presentation point"
        );
        assert_eq!(
            track.segment_duration.get(),
            u64::try_from(track.first_segment_boundary_pts - track.segmentation_origin_pts)
                .expect("the selected boundary follows the segment origin")
        );
    }

    /// One second of video starting at `start_seconds`.
    fn sample(start_seconds: i64, random_access: bool, payload_bytes: usize) -> NormalizedSample {
        crate::media::fixtures::video_sample(
            start_seconds * VIDEO_SECOND,
            VIDEO_SECOND as u64,
            random_access,
            payload_bytes,
        )
    }

    fn policy() -> SegmentationPolicy {
        SegmentationPolicy::latency_first(Duration::from_secs(10), Duration::from_millis(200))
    }

    /// A byte budget expressed as room for `slots` single-byte samples.
    ///
    /// Written this way because the budget is charged at retained cost, so a
    /// raw byte count would encode `size_of::<NormalizedSample>()` into every
    /// expectation and break whenever a field is added.
    fn budget(slots: usize) -> usize {
        slots * (size_of::<NormalizedSample>() + 1)
    }

    fn limits(slots: usize, maximum_media_duration: Duration) -> PrerollLimits {
        PrerollLimits {
            maximum_buffered_bytes: budget(slots),
            maximum_buffered_samples: usize::MAX,
            maximum_wall_time: Duration::from_secs(1),
            maximum_media_duration,
        }
    }

    async fn preroll(
        source: &mut dyn SampleSource,
        limits: PrerollLimits,
        events: &EventSink,
    ) -> Result<Preroll, PrerollError> {
        let presentation = presentation();
        let timeline = timeline();
        run(
            source,
            PrerollRequest {
                presentation: &presentation,
                timeline: &timeline,
                limits,
                policy: policy(),
            },
            events,
        )
        .await
    }

    fn sink() -> EventSink {
        Events::default().scoped(SessionId(nz::u64!(1)))
    }

    #[tokio::test]
    async fn locks_once_and_retains_every_observed_sample() {
        let recorder = Arc::new(Recorder::default());
        let events = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>)
            .scoped(SessionId(nz::u64!(1)));
        let mut source = Replay::new(
            (0..10)
                .map(|second| vec![sample(second, second == 8, 1)])
                .collect(),
        );

        let preroll = preroll(&mut source, limits(10, Duration::from_secs(20)), &events)
            .await
            .expect("pre-roll succeeds");

        assert_eq!(preroll.buffered.len(), 10);
        assert_eq!(
            preroll
                .segmentation
                .get(TrackId(0))
                .map(|track| track.segment_duration.get()),
            Some(8 * 90_000)
        );
        assert_eq!(
            preroll
                .segmentation
                .get(TrackId(0))
                .map(|track| track.part_duration.get()),
            Some(90_000)
        );

        let observed = recorder.events.lock();
        assert_eq!(observed.len(), 1);
        assert!(matches!(
            observed.first(),
            Some(SessionEvent::SegmentationLocked { aligned: true, .. })
        ));
    }

    #[tokio::test]
    async fn a_whole_batch_of_samples_is_admitted_in_order() {
        let mut source = Replay::new(vec![
            (0..5).map(|second| sample(second, false, 1)).collect(),
            (5..10)
                .map(|second| sample(second, second == 8, 1))
                .collect(),
        ]);

        let preroll = preroll(&mut source, limits(10, Duration::from_secs(20)), &sink())
            .await
            .expect("pre-roll succeeds");

        assert_eq!(preroll.buffered.len(), 10);
        assert_eq!(preroll.buffered[0].pts(), 0);
        assert_eq!(preroll.buffered[9].pts(), 9 * 90_000);
    }

    #[tokio::test]
    async fn rejects_when_buffered_bytes_exceed_the_limit() {
        let mut source = Replay::new(vec![vec![sample(8, true, 3)]]);

        let error = preroll(&mut source, limits(1, Duration::from_secs(20)), &sink())
            .await
            .expect_err("byte limit rejects pre-roll");

        assert_eq!(error, PrerollError::LimitExceeded);
    }

    #[tokio::test]
    async fn empty_access_units_are_charged_for_the_room_they_occupy() {
        // Zero-length payloads that never advance the clock: under a budget
        // charged on payload size alone, this buffers until the node dies.
        let mut source = Replay::new(vec![
            (0..64)
                .map(|_| crate::media::fixtures::video_sample(0, 0, false, 0))
                .collect(),
        ]);

        let error = preroll(&mut source, limits(8, Duration::from_secs(20)), &sink())
            .await
            .expect_err("structural cost rejects a flood of empty samples");

        assert_eq!(error, PrerollError::LimitExceeded);
    }

    #[tokio::test]
    async fn rejects_when_the_retained_sample_count_exceeds_the_limit() {
        let mut source = Replay::new(vec![
            (0..10)
                .map(|second| sample(second, second == 8, 1))
                .collect(),
        ]);
        let mut limits = limits(1_000, Duration::from_secs(20));
        limits.maximum_buffered_samples = 4;

        let error = preroll(&mut source, limits, &sink())
            .await
            .expect_err("the sample-count limit rejects pre-roll");

        assert_eq!(error, PrerollError::LimitExceeded);
    }

    #[tokio::test]
    async fn rejects_when_media_duration_exceeds_the_limit() {
        let mut source = Replay::new(vec![vec![sample(8, true, 1)]]);

        let error = preroll(&mut source, limits(10, Duration::from_secs(5)), &sink())
            .await
            .expect_err("media-duration limit rejects pre-roll");

        assert_eq!(error, PrerollError::LimitExceeded);
    }

    #[tokio::test]
    async fn rejects_when_wall_time_expires_while_waiting_for_input() {
        let mut source = Stalled;

        let error = preroll(
            &mut source,
            PrerollLimits {
                maximum_wall_time: Duration::from_millis(1),
                ..limits(10, Duration::from_secs(20))
            },
            &sink(),
        )
        .await
        .expect_err("wall-time limit rejects stalled pre-roll");

        assert_eq!(error, PrerollError::LimitExceeded);
    }

    #[tokio::test]
    async fn rejects_after_the_horizon_without_a_random_access_boundary() {
        let mut source = Replay::new(vec![vec![sample(9, false, 1)]]);

        let error = preroll(&mut source, limits(10, Duration::from_secs(20)), &sink())
            .await
            .expect_err("missing boundary rejects pre-roll");

        assert_eq!(
            error,
            PrerollError::BoundarySelection(BoundarySelectionError::NoCompatibleBoundary)
        );
    }

    #[tokio::test]
    async fn rejects_when_the_input_ends_before_a_boundary_is_provable() {
        let mut source = Replay::new(vec![vec![sample(0, false, 1)]]);

        let error = preroll(&mut source, limits(10, Duration::from_secs(20)), &sink())
            .await
            .expect_err("truncated input rejects pre-roll");

        assert_eq!(error, PrerollError::NoSegmentationBoundary);
    }
}
