use tokio::time::{Instant, timeout};

use crate::{
    domain::{TickTimestamp, TrackId},
    media::{NormalizedSample, PresentationPlan, Rounding, SampleSource, TimelineCalibration},
    observe::{EventSink, SessionEvent},
    source::InputState,
};

use super::{
    CadenceError, CadenceObserver, PrerollError, PrerollLimits, SegmentationPlan,
    SegmentationPolicy,
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

    let mut observer = CadenceObserver::new(presentation, timeline, policy)?;
    let horizons = media_horizons(timeline, limits)?;
    let mut buffered = Vec::with_capacity(INITIAL_BUFFER_SAMPLES);
    let mut buffered_bytes = 0_usize;
    let deadline = Instant::now() + limits.maximum_wall_time;
    let mut work = 0_usize;

    loop {
        if let Some(segmentation) = select(
            &observer,
            presentation,
            &buffered,
            policy,
            limits,
            &mut work,
        )? {
            return Ok(lock(segmentation, buffered, InputState::Open, events));
        }
        if observer.exhausted() {
            return Err(CadenceError::NoSegmentationBoundary.into());
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
                &mut observer,
                &horizons,
                &mut buffered_bytes,
                retained,
                limits,
                sample,
            )?;
        }

        if !state.is_open() {
            // The input is gone, so no further evidence can arrive. Whatever
            // the observer can conclude now is final.
            let segmentation = select(
                &observer,
                presentation,
                &buffered,
                policy,
                limits,
                &mut work,
            )?
            .ok_or(CadenceError::NoSegmentationBoundary)?;
            return Ok(lock(segmentation, buffered, state, events));
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
                .ok_or(CadenceError::HorizonOverflow(track.track_id))?;
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
    observer: &mut CadenceObserver,
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
        .ok_or(CadenceError::UnknownTrack(track_id))?;
    let end = sample
        .pts()
        .checked_add_unsigned(sample.duration())
        .ok_or(CadenceError::TimestampOverflow(track_id))?;
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

    observer.observe(sample)?;
    *buffered_bytes = total;
    Ok(())
}

/// An infeasible candidate is not a malformed publisher: try the next boundary.
fn select(
    observer: &CadenceObserver,
    presentation: &PresentationPlan,
    buffered: &[NormalizedSample],
    policy: SegmentationPolicy,
    limits: PrerollLimits,
    work: &mut usize,
) -> Result<Option<SegmentationPlan>, PrerollError> {
    let mut selected = None;
    let result = observer.visit_candidates(work, |tracks, work| {
        let mut segmentation = SegmentationPlan::new(presentation, tracks)?;
        segmentation.early_boundary = policy.early_boundary;
        segmentation.late_boundary = policy.late_boundary;
        segmentation.limits = limits;
        match super::replay::admit(presentation, &mut segmentation, buffered, policy, work) {
            Ok(()) => {
                selected = Some(segmentation);
                return Ok(true);
            }
            Err(
                PrerollError::Cadence(
                    CadenceError::NoSegmentationBoundary | CadenceError::InconsistentPartCadence(_),
                )
                | PrerollError::Packaging(
                    crate::mux::MuxError::BoundaryWindow { .. }
                    | crate::mux::MuxError::Boundary {
                        reason: "segment ceiling exhausted",
                        ..
                    },
                ),
            ) => {}
            Err(error) => return Err(error),
        }
        Ok(false)
    });
    match result {
        Err(PrerollError::Cadence(CadenceError::WorkLimitExceeded)) => {
            Err(PrerollError::LimitExceeded)
        }
        Err(error) => Err(error),
        Ok(()) => Ok(selected),
    }
}

fn lock(
    segmentation: SegmentationPlan,
    buffered: Vec<NormalizedSample>,
    input_state: InputState,
    events: &EventSink,
) -> Preroll {
    events.emit(SessionEvent::SegmentationLocked {
        segment: segmentation.longest_segment_duration(),
        part: segmentation.shortest_part_duration(),
    });
    Preroll {
        segmentation,
        input_state,
        buffered,
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

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

    use crate::segment::fixtures::SampleBatches;

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
        SegmentationPolicy::latency_first(Duration::from_secs(10), Duration::from_secs(1))
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
        let mut source = SampleBatches::new(
            (0..=10)
                .map(|second| vec![sample(second, second == 0 || second == 8, 1)])
                .collect(),
        );

        let preroll = preroll(&mut source, limits(11, Duration::from_secs(20)), &events)
            .await
            .expect("pre-roll succeeds");

        assert_eq!(preroll.buffered.len(), 11);
        let track = preroll
            .segmentation
            .get(TrackId(0))
            .expect("the plan covers the track");
        assert_eq!(track.segment_duration.get(), 8 * 90_000);
        assert_eq!(track.part_duration.get(), 90_000);

        let observed = recorder.events.lock();
        assert_eq!(observed.len(), 1);
        assert!(matches!(
            observed.first(),
            Some(SessionEvent::SegmentationLocked { .. })
        ));
    }

    #[tokio::test]
    async fn a_whole_batch_of_samples_is_admitted_in_order() {
        let mut source = SampleBatches::new(vec![
            (0..5)
                .map(|second| sample(second, second == 0, 1))
                .collect(),
            (5..=10)
                .map(|second| sample(second, second == 0 || second == 8, 1))
                .collect(),
        ]);

        let preroll = preroll(&mut source, limits(11, Duration::from_secs(20)), &sink())
            .await
            .expect("pre-roll succeeds");

        assert_eq!(preroll.buffered.len(), 11);
        assert_eq!(preroll.buffered[0].pts(), 0);
        assert_eq!(preroll.buffered[10].pts(), 10 * 90_000);
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
        let mut source = SampleBatches::new(vec![
            (1..24)
                .map(|second| sample(second, (second - 1) % 4 == 0, 1))
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

    #[tokio::test]
    async fn rejects_when_buffered_bytes_exceed_the_limit() {
        let mut source = SampleBatches::new(vec![vec![sample(8, true, 3)]]);

        let error = preroll(&mut source, limits(1, Duration::from_secs(20)), &sink())
            .await
            .expect_err("byte limit rejects pre-roll");

        assert_eq!(error, PrerollError::LimitExceeded);
    }

    #[tokio::test]
    async fn empty_access_units_are_charged_for_the_room_they_occupy() {
        // Zero-length payloads that never advance the clock: under a budget
        // charged on payload size alone, this buffers until the node dies.
        let mut source = SampleBatches::new(vec![
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
        let mut source = SampleBatches::new(vec![
            (0..10)
                .map(|second| sample(second, second == 0 || second == 8, 1))
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
        let mut source = SampleBatches::new(vec![vec![sample(8, true, 1)]]);

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
        let mut source = SampleBatches::new(vec![
            (0..=10).map(|second| sample(second, false, 1)).collect(),
        ]);

        let error = preroll(&mut source, limits(20, Duration::from_secs(20)), &sink())
            .await
            .expect_err("missing boundary rejects pre-roll");

        assert_eq!(
            error,
            PrerollError::Cadence(CadenceError::NoSegmentationBoundary)
        );
    }

    #[tokio::test]
    async fn rejects_when_the_input_ends_before_a_boundary_is_provable() {
        let mut source = SampleBatches::new(vec![vec![sample(0, true, 1)]]);

        let error = preroll(&mut source, limits(10, Duration::from_secs(20)), &sink())
            .await
            .expect_err("truncated input rejects pre-roll");

        assert_eq!(
            error,
            PrerollError::Cadence(CadenceError::NoSegmentationBoundary)
        );
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::media::fixtures::{video_presentation, video_sample, video_timeline};
    use crate::segment::fixtures::SampleBatches;
    use std::time::Duration;
    #[tokio::test]
    async fn bounded_admission_selects_three_seconds_without_runtime_slack()
    -> Result<(), Box<dyn std::error::Error>> {
        let presentation = video_presentation();
        let timeline = video_timeline();
        let events =
            crate::observe::Events::default().scoped(crate::domain::SessionId(nz::u64!(1)));
        for hard in [false, true] {
            let mut source = SampleBatches::new(vec![
                (0..=3)
                    .map(|second| {
                        video_sample(second * 90_000, 90_000, second == 0 || second == 3, 1)
                    })
                    .collect(),
            ]);
            let mut policy =
                SegmentationPolicy::latency_first(Duration::from_secs(2), Duration::from_secs(1));
            if hard {
                policy.maximum_segment_duration = policy.desired_segment_duration;
                policy.maximum_part_duration = Duration::from_secs(1);
            }
            let result = run(
                &mut source,
                PrerollRequest {
                    presentation: &presentation,
                    timeline: &timeline,
                    limits: PrerollLimits::permissive(),
                    policy,
                },
                &events,
            )
            .await;
            if hard {
                assert!(result.is_err());
            } else {
                let admitted = result?;
                assert_eq!(
                    admitted.segmentation.longest_segment_duration(),
                    Duration::from_secs(3)
                );
                assert_eq!(admitted.segmentation.late_boundary, Duration::ZERO);
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn hard_admission_accepts_exact_cfr_without_frame_slack()
    -> Result<(), Box<dyn std::error::Error>> {
        let presentation = video_presentation();
        let timeline = video_timeline();
        let events =
            crate::observe::Events::default().scoped(crate::domain::SessionId(nz::u64!(1)));
        let mut source = SampleBatches::new(vec![
            (0..=60)
                .map(|frame| video_sample(frame * 3_000, 3_000, frame % 60 == 0, 1))
                .collect(),
        ]);
        let mut policy =
            SegmentationPolicy::latency_first(Duration::from_secs(2), Duration::from_secs(1));
        policy.maximum_segment_duration = policy.desired_segment_duration;
        policy.maximum_part_duration = Duration::from_secs(1);
        let admitted = run(
            &mut source,
            PrerollRequest {
                presentation: &presentation,
                timeline: &timeline,
                limits: PrerollLimits::permissive(),
                policy,
            },
            &events,
        )
        .await?;
        let track = admitted.segmentation.iter().next().expect("video plan");
        assert_eq!(track.maximum_segment_ticks(Duration::ZERO), 180_000);
        assert_eq!(track.boundary_tolerance, 0);
        Ok(())
    }

    #[tokio::test]
    async fn timing_replay_selects_a_larger_part_only_when_needed()
    -> Result<(), Box<dyn std::error::Error>> {
        let presentation = video_presentation();
        let timeline = video_timeline();
        let events =
            crate::observe::Events::default().scoped(crate::domain::SessionId(nz::u64!(1)));
        let mut source = SampleBatches::new(vec![
            (0..=4)
                .map(|frame| video_sample(frame * 5_400, 5_400, frame % 4 == 0, 1))
                .collect(),
        ]);
        let policy = SegmentationPolicy::latency_first(
            Duration::from_millis(240),
            Duration::from_millis(100),
        );
        let admitted = run(
            &mut source,
            PrerollRequest {
                presentation: &presentation,
                timeline: &timeline,
                limits: PrerollLimits::permissive(),
                policy,
            },
            &events,
        )
        .await?;
        assert_eq!(
            admitted.segmentation.shortest_part_duration(),
            Duration::from_millis(120)
        );
        Ok(())
    }
    #[tokio::test]
    async fn exact_audio_cap_selects_an_earlier_feasible_access_unit()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            domain::{MediaKind, Timebase, fixtures::TrackBuilder},
            media::fixtures,
        };
        let tb = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let presentation = fixtures::presentation(vec![
            TrackBuilder::new(0, MediaKind::Audio).timebase(tb).build(),
        ]);
        let timeline = fixtures::timeline([(0, tb)]);
        let mut policy =
            SegmentationPolicy::latency_first(Duration::from_secs(6), Duration::from_secs(1));
        policy.maximum_segment_duration = Duration::from_secs(6);
        policy.maximum_part_duration = Duration::from_secs(1);
        let mut source = SampleBatches::new(vec![
            (0..283)
                .map(|i| fixtures::audio_sample(0, i * 1024, 1024))
                .collect(),
        ]);
        let admitted = run(
            &mut source,
            PrerollRequest {
                presentation: &presentation,
                timeline: &timeline,
                limits: PrerollLimits::permissive(),
                policy,
            },
            &crate::mux::fixtures::discarded_events(),
        )
        .await?;
        let track = admitted.segmentation.iter().next().expect("audio plan");
        assert_eq!(track.segment_duration.get(), 286_720);
        assert!(track.maximum_segment_ticks(Duration::ZERO) <= 288_000);
        let mut observer = CadenceObserver::new(&presentation, &timeline, policy)?;
        for sample in &admitted.buffered {
            observer.observe(sample)?;
        }
        assert!(matches!(
            select(
                &observer,
                &presentation,
                &admitted.buffered,
                policy,
                PrerollLimits::permissive(),
                &mut 999_999
            ),
            Err(PrerollError::LimitExceeded)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn audio_rounding_falls_back_to_an_earlier_common_video_boundary()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            domain::{AudioTrim, MediaKind, MediaParameters, Timebase, fixtures::TrackBuilder},
            media::fixtures,
        };
        for (padding, delay, jitter) in [(0_u32, 0_i64, 0_u64), (312, 0, 0), (0, 4800, 100)] {
            let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
            let second_video_base = Timebase::new(nz::u32!(1), nz::u32!(30_000));
            let mut audio = TrackBuilder::new(1, MediaKind::Audio)
                .timebase(audio_base)
                .first_pts(Some(delay))
                .build();
            if let MediaParameters::Audio { timing, .. } = &mut audio.parameters {
                timing.initial_padding_samples = padding;
            }
            let presentation = fixtures::presentation(vec![
                TrackBuilder::new(0, MediaKind::Video).build(),
                audio,
                TrackBuilder::new(2, MediaKind::Video)
                    .timebase(second_video_base)
                    .build(),
            ]);
            let timeline = fixtures::timeline([
                (0, Timebase::hz90k()),
                (1, audio_base),
                (2, second_video_base),
            ]);
            let mut samples = Vec::new();
            for frame in 0..=181 {
                samples.push(video_sample(frame * 3000, 3000, frame % 60 == 0, 0));
                let mut second = video_sample(frame * 1000, 1000, frame % 60 == 0, 0);
                if let NormalizedSample::Video(video) = &mut second {
                    video.track_id = crate::domain::TrackId(2);
                }
                samples.push(second);
            }
            for frame in 0..284 {
                let mut sample =
                    fixtures::audio_sample(1, delay - i64::from(padding) + frame * 1024, 1024);
                if frame == 0
                    && let NormalizedSample::Audio(audio) = &mut sample
                {
                    audio.trim = AudioTrim {
                        leading_samples: padding,
                        trailing_samples: 0,
                    };
                }
                samples.push(sample);
            }
            samples.sort_by_key(|sample| {
                sample.pts()
                    * match sample.track_id().0 {
                        0 => 8,
                        1 => 15,
                        _ => 24,
                    }
            });
            let mut policy =
                SegmentationPolicy::latency_first(Duration::from_secs(6), Duration::from_secs(1));
            policy.maximum_segment_duration = Duration::from_secs(6);
            policy.maximum_part_duration = Duration::from_secs(1);
            policy.early_boundary = Duration::from_millis(jitter);
            policy.late_boundary = policy.early_boundary;
            let (video_samples, audio_samples): (Vec<_>, Vec<_>) = samples
                .into_iter()
                .partition(|sample| sample.track_id().0 != 1);
            let admitted = run(
                &mut SampleBatches::new(vec![video_samples, audio_samples]),
                PrerollRequest {
                    presentation: &presentation,
                    timeline: &timeline,
                    limits: PrerollLimits::permissive(),
                    policy,
                },
                &crate::mux::fixtures::discarded_events(),
            )
            .await?;
            assert_eq!(
                admitted
                    .segmentation
                    .get(crate::domain::TrackId(0))
                    .expect("video")
                    .segment_duration
                    .get(),
                360_000
            );
            for track in admitted.segmentation.iter() {
                assert!(
                    track.timebase.ticks_to_duration(
                        track.maximum_segment_ticks(policy.early_boundary + policy.late_boundary)
                    ) <= policy.segment_cap()
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn repaired_vfr_parts_pass_admission_cmaf_and_hls_storage()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            delivery::hls::{PublisherFactory, StorePublisherFactory, StreamStore},
            domain::{MediaKind, Payload, StreamId, fixtures::TrackBuilder},
            media::fixtures,
            mux::{
                MuxerFactory, MuxerStartRequest, PackagedMedia, PassThroughMuxerFactory,
                fixtures::{H264_EXTRADATA, H264_IDR, H264_P},
            },
        };
        let presentation = fixtures::presentation(vec![
            TrackBuilder::new(0, MediaKind::Video)
                .codec_extradata(H264_EXTRADATA)
                .build(),
        ]);
        let timeline = video_timeline();
        let samples = [
            (0, 20, true),
            (20, 55, false),
            (75, 45, false),
            (120, 60, false),
            (180, 20, true),
        ]
        .into_iter()
        .map(|(pts, duration, rap)| {
            let mut sample = video_sample(pts * 90, duration * 90, rap, 0);
            if let NormalizedSample::Video(video) = &mut sample {
                video.payload = Payload::from(if rap { H264_IDR } else { H264_P });
            }
            sample
        })
        .collect();
        let mut policy = SegmentationPolicy::latency_first(
            Duration::from_millis(180),
            Duration::from_millis(100),
        );
        policy.maximum_segment_duration = Duration::from_millis(180);
        policy.maximum_part_duration = Duration::from_millis(100);
        let events = crate::mux::fixtures::discarded_events();
        let admitted = run(
            &mut SampleBatches::new(vec![samples]),
            PrerollRequest {
                presentation: &presentation,
                timeline: &timeline,
                limits: PrerollLimits::permissive(),
                policy,
            },
            &events,
        )
        .await?;
        assert_eq!(
            admitted.segmentation.shortest_part_duration(),
            Duration::from_millis(100)
        );
        let mut corrupt = admitted.buffered.clone();
        if let NormalizedSample::Video(video) = &mut corrupt[0] {
            video.codec = crate::domain::Codec::Hevc;
        }
        let error = run(
            &mut SampleBatches::new(vec![corrupt]),
            PrerollRequest {
                presentation: &presentation,
                timeline: &timeline,
                limits: PrerollLimits::permissive(),
                policy,
            },
            &events,
        )
        .await
        .expect_err("malformed media is not a candidate rejection");
        assert!(matches!(
            error,
            PrerollError::Packaging(crate::mux::MuxError::Mux(_))
        ));

        let mut started = PassThroughMuxerFactory.start(MuxerStartRequest {
            presentation: &presentation,
            segmentation: &admitted.segmentation,
            time_anchor: std::time::SystemTime::UNIX_EPOCH,
            events: &events,
        })?;
        let store = StreamStore::default();
        let stream = StreamId::new("repair");
        let mut publisher =
            StorePublisherFactory::new(store.clone()).start(&stream, started.presentation)?;
        let mut output = Vec::new();
        for sample in admitted.buffered {
            started.muxer.push(sample, &mut output)?;
        }
        let parts: Vec<_> = output
            .iter()
            .filter_map(|media| match media {
                PackagedMedia::Chunk(chunk) => Some((chunk.duration, chunk.independent)),
                _ => None,
            })
            .collect();
        assert_eq!(parts, [(1800, true), (9000, false), (5400, false)]);
        for media in output {
            publisher.write(media)?;
        }
        assert!(store.get(&stream).is_some());
        Ok(())
    }
}
