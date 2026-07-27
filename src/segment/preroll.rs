use std::num::NonZero;

use tokio::time::{Instant, timeout};

use crate::{
    domain::{TickDuration, TickTimestamp, TrackId, duration_since},
    media::{NormalizedSample, PresentationPlan, SampleSource, TimelineCalibration, TrackTimeline},
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

    let mut selector = BoundarySelector::new(timeline, policy)?;
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
            let duration = track
                .timebase
                .duration_to_ticks_ceil(limits.maximum_media_duration);
            let maximum_pts = track
                .origin_pts
                .checked_add_unsigned(duration)
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
        tracks.push(plan_track(&buffered, policy, timing, boundary.pts)?);
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
    boundary_pts: TickTimestamp,
) -> Result<TrackSegmentationPlan, PrerollError> {
    let segment_duration = duration_since(boundary_pts, timing.origin_pts)
        .and_then(NonZero::<TickDuration>::new)
        .ok_or(PrerollError::NoSegmentationBoundary)?;
    let part_duration = select_part_duration(
        buffered,
        timing.track_id,
        timing.origin_pts,
        boundary_pts,
        timing
            .timebase
            .duration_to_ticks(policy.desired_part_duration),
    )
    .ok_or(PrerollError::NoPartDuration(timing.track_id))?;

    Ok(TrackSegmentationPlan {
        track_id: timing.track_id,
        timebase: timing.timebase,
        segment_duration,
        part_duration,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc, time::Duration};

    use parking_lot::Mutex;

    use crate::{
        domain::{Appender, BoxFuture, SessionId},
        media::{
            MediaError,
            fixtures::{VIDEO_SECOND, video_presentation as presentation, video_timeline as timeline},
        },
        observe::{EventObserver, Events, SessionEvent},
    };

    use super::*;

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
