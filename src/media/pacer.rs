use std::{cmp::Ordering, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::time::Instant;

use crate::{
    admission::{Ceiling, Floor, Pace},
    domain::{MediaInstant, TrackId},
    observe::{EventSink, MediaMeters, SessionEvent},
};

use super::{NormalizedMedia, TimelineCalibration};

struct PacingWait<'a> {
    meters: &'a dyn MediaMeters,
    drift: &'a mut DriftMonitor,
    started: Instant,
    lead: Duration,
    completed: bool,
}

impl<'a> PacingWait<'a> {
    fn new(meters: &'a dyn MediaMeters, drift: &'a mut DriftMonitor, lead: Duration) -> Self {
        meters.pacing_observation(lead, Duration::ZERO, true);
        Self {
            meters,
            drift,
            started: Instant::now(),
            lead,
            completed: false,
        }
    }

    fn complete(&mut self, media_lead: Duration) {
        let elapsed = Instant::now().saturating_duration_since(self.started);
        self.meters.pacing_observation(media_lead, elapsed, false);
        self.drift.withhold(elapsed);
        self.completed = true;
    }
}

impl Drop for PacingWait<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let elapsed = Instant::now().saturating_duration_since(self.started);
            self.meters
                .pacing_observation(self.lead.saturating_sub(elapsed), elapsed, true);
            self.drift.withhold(elapsed);
        }
    }
}

/// A sample's presentation instant, remembering which track it came from.
///
/// [`MediaInstant`] does the arithmetic and reports overflow as [`None`]; the
/// track identity lives here so a failure can name the publisher's track rather
/// than an anonymous timestamp.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MediaPoint {
    track_id: TrackId,
    instant: MediaInstant,
}

impl MediaPoint {
    fn compare(self, other: Self) -> Result<Ordering, PacingError> {
        self.instant
            .compare(other.instant)
            .ok_or(PacingError::TimestampOverflow(self.track_id))
    }

    pub(crate) fn elapsed_since(self, earlier: Self) -> Result<Duration, PacingError> {
        self.instant
            .elapsed_since(earlier.instant)
            .ok_or(PacingError::TimestampOverflow(self.track_id))
    }
}

fn point(
    sample: &NormalizedMedia,
    timeline: &TimelineCalibration,
) -> Result<MediaPoint, PacingError> {
    let track_id = sample.track_id();
    let track = timeline
        .get(track_id)
        .ok_or(PacingError::UnknownTrack(track_id))?;
    Ok(MediaPoint {
        track_id,
        instant: MediaInstant::new(track.timebase, sample.pts(), track.origin_pts),
    })
}

/// A monotonic media-time watermark across track-native timebases.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MediaWatermark(Option<MediaPoint>);

impl MediaWatermark {
    pub(crate) fn observe(
        &mut self,
        sample: &NormalizedMedia,
        timeline: &TimelineCalibration,
    ) -> Result<bool, PacingError> {
        let candidate = point(sample, timeline)?;
        match self.0 {
            Some(current) if candidate.compare(current)? != Ordering::Greater => Ok(false),
            _ => {
                self.0 = Some(candidate);
                Ok(true)
            }
        }
    }

    pub(crate) fn get(self) -> Option<MediaPoint> {
        self.0
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PacingError {
    #[error("cannot pace an unknown track {0}")]
    UnknownTrack(TrackId),
    #[error("timestamp arithmetic overflowed while pacing {0}")]
    TimestampOverflow(TrackId),
    #[error(
        "publisher advanced {media:?} of media across {window:?} of wall clock, below the \
         required minimum of {required:?}"
    )]
    BelowFloor {
        window: Duration,
        media: Duration,
        required: Duration,
    },
}

/// Schedules media against wall-clock deadlines after the startup burst.
///
/// Deadlines advance with media, not with the completion of each sleep. Time
/// spent muxing, storing, reading input, or servicing supervision therefore
/// reduces the next wait instead of making the stream progressively slower.
#[derive(Clone, Copy)]
struct CeilingClock {
    ceiling: Ceiling,
    started_at: Instant,
    /// Virtual media position. Rebased after lateness so a stalled publisher
    /// can resume but cannot bank the entire stall as a catch-up allowance.
    scheduled: Duration,
}

impl CeilingClock {
    fn new(ceiling: Ceiling, now: Instant) -> Self {
        Self {
            ceiling,
            started_at: now,
            scheduled: Duration::ZERO,
        }
    }

    /// Plans against a copy; a cancelled wait must not spend media twice.
    fn schedule(mut self, media: Duration, now: Instant) -> Option<(Self, Instant)> {
        let wall_media = self
            .ceiling
            .pace
            .media_for(now.saturating_duration_since(self.started_at));
        // Admit an overdue sample immediately, then retain at most `burst`
        // for the following samples. With no burst, the next advance is due
        // one media interval later, however long the preceding stall lasted.
        self.scheduled = self.scheduled.checked_add(media)?.max(wall_media);
        let wall_due = self
            .ceiling
            .pace
            .wall_for(self.scheduled.saturating_sub(self.ceiling.burst));
        let due = self.started_at.checked_add(wall_due)?;
        Some((self, due))
    }

    fn available(self, now: Instant) -> Duration {
        let wall_media = self
            .ceiling
            .pace
            .media_for(now.saturating_duration_since(self.started_at));
        self.ceiling
            .burst
            .saturating_sub(self.scheduled.saturating_sub(wall_media))
    }
}

/// Enforces a [`Floor`]: whether a publisher sustains the minimum rate its
/// policy requires.
///
/// Sliding rather than fixed windows would need a history of samples; this
/// keeps one accumulator per window and only judges a window once it has run
/// in full, which also makes the first window startup grace by construction.
struct FloorWindow {
    floor: Floor,
    started_at: Instant,
    media: Duration,
}

impl FloorWindow {
    fn new(floor: Floor, now: Instant) -> Self {
        Self {
            floor,
            started_at: now,
            media: Duration::ZERO,
        }
    }

    /// Adds media-time progress and judges the window if it has elapsed.
    fn observe(&mut self, media: Duration, now: Instant) -> Result<(), PacingError> {
        self.media = self.media.saturating_add(media);
        let elapsed = now.saturating_duration_since(self.started_at);
        if elapsed < self.floor.window {
            return Ok(());
        }

        let required = self.floor.pace.media_for(elapsed);
        if self.media < required {
            return Err(PacingError::BelowFloor {
                window: elapsed,
                media: self.media,
                required,
            });
        }
        self.started_at = now;
        self.media = Duration::ZERO;
        Ok(())
    }
}

/// Reports whether media time is keeping up with wall clock.
///
/// Deliberately toothless. A publisher falling behind realtime is ended by
/// `floor` or by nothing at all — that is the division the deleted publication
/// deadline violated — but an operator who configured no floor still wants to
/// hear that their live stream is drifting. So this only ever emits.
///
/// Time this node spent holding the publisher at a ceiling is withheld from
/// the clock. Otherwise a 1x ceiling plus ordinary mux work reads as a slow
/// encoder, which is the publisher being blamed for an instruction it obeyed.
///
/// The same fixed-window shape as [`FloorWindow`], for the same reason: one
/// accumulator, judged once per window, with the first window as grace.
struct DriftMonitor {
    started_at: Instant,
    media: Duration,
    /// Wall time already spent in ceiling waits during this window.
    withheld: Duration,
    /// Whether the last judged window was reported as behind, so each
    /// transition is announced once rather than every window. Without this a
    /// publisher that stays behind logs on a timer forever.
    behind: bool,
}

impl DriftMonitor {
    /// How long a window is judged over.
    ///
    /// Compiled rather than configured: this reports, so its sensitivity is
    /// not an operator's tradeoff to make. Long enough that a GOP boundary or
    /// a retransmit does not read as drift.
    const WINDOW: Duration = Duration::from_secs(10);

    /// How far below realtime a window must fall to count as behind.
    ///
    /// The hysteresis gap is the point: a publisher hovering at exactly
    /// realtime would otherwise alternate every window. Recovering needs 95%
    /// of realtime, while falling behind needs to drop under 90%.
    const BEHIND: Pace = Pace::new(nz::u32!(9), nz::u32!(10));
    const RECOVERED: Pace = Pace::new(nz::u32!(19), nz::u32!(20));

    fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            media: Duration::ZERO,
            withheld: Duration::ZERO,
            behind: false,
        }
    }

    /// Credits wall time this node spent holding the publisher at its ceiling.
    fn withhold(&mut self, waited: Duration) {
        self.withheld = self.withheld.saturating_add(waited);
    }

    /// Adds media-time progress and reports a change of state, if any.
    fn observe(&mut self, media: Duration, now: Instant, events: &EventSink) {
        self.media = self.media.saturating_add(media);
        let wall = now.saturating_duration_since(self.started_at);
        if wall < Self::WINDOW {
            return;
        }
        // Judged against the clock the publisher was allowed to use, not the
        // one this node paused.
        let elapsed = wall.saturating_sub(self.withheld);
        let observed = self.media;
        self.started_at = now;
        self.media = Duration::ZERO;
        self.withheld = Duration::ZERO;

        if self.behind {
            if observed >= Self::RECOVERED.media_for(elapsed) {
                self.behind = false;
                events.emit(SessionEvent::PublisherTrackingRealtime);
            }
        } else if observed < Self::BEHIND.media_for(elapsed) {
            self.behind = true;
            events.emit(SessionEvent::PublisherBehindRealtime {
                window: elapsed,
                media: observed,
            });
        }
    }
}

/// Maps normalized media time onto a monotonic wall clock without rewriting it.
pub struct MediaPacer {
    /// Absent means unthrottled: media is admitted as fast as it arrives.
    ceiling: Option<CeilingClock>,
    /// Absent means no minimum rate is required.
    floor: Option<FloorWindow>,
    /// Always present: drift is reported whether or not a bound is set.
    drift: DriftMonitor,
    timeline: TimelineCalibration,
    watermark: MediaWatermark,
    meters: Arc<dyn MediaMeters>,
}

impl MediaPacer {
    /// Starts after pre-roll, treating all buffered planning media as the
    /// bounded startup burst.
    pub fn after_preroll(
        ceiling: Option<Ceiling>,
        floor: Option<Floor>,
        timeline: &TimelineCalibration,
        buffered: &[NormalizedMedia],
        meters: Arc<dyn MediaMeters>,
    ) -> Result<Self, PacingError> {
        let mut watermark = MediaWatermark::default();
        for sample in buffered {
            watermark.observe(sample, timeline)?;
        }
        let now = Instant::now();
        Ok(Self {
            ceiling: ceiling.map(|ceiling| CeilingClock::new(ceiling, now)),
            floor: floor.map(|floor| FloorWindow::new(floor, now)),
            drift: DriftMonitor::new(now),
            timeline: timeline.clone(),
            watermark,
            meters,
        })
    }

    /// Delays a sample until its media time has been earned.
    ///
    /// A sleeping call stops the live loop from draining the next bounded
    /// input batch, which is the entire backpressure mechanism: a publisher
    /// cannot push media into the process faster than this returns.
    pub async fn pace(
        &mut self,
        sample: &NormalizedMedia,
        events: &EventSink,
    ) -> Result<(), PacingError> {
        let previous = self.watermark.get();
        // Work against a copy and commit only after any sleep finishes. The
        // supervision loop may cancel this future to service a stop or health
        // tick; committing first would let the retry mistake an unpaced sample
        // for an old B-frame and publish it immediately.
        let mut next_watermark = self.watermark;
        if !next_watermark.observe(sample, &self.timeline)? {
            return Ok(());
        }
        let current = next_watermark
            .get()
            .expect("observing an advancing sample establishes a watermark");

        // How far this sample advances the timeline. The first advancing
        // sample after pre-roll has nothing to measure from and so costs
        // nothing, which keeps the configured burst as the only additional head
        // start beyond pre-roll.
        let advance = match previous {
            Some(previous) => current.elapsed_since(previous)?,
            None => Duration::ZERO,
        };

        let now = Instant::now();
        if let Some(ceiling) = self.ceiling {
            let (next, due) = ceiling
                .schedule(advance, now)
                .ok_or(PacingError::TimestampOverflow(current.track_id))?;
            let delay = due.saturating_duration_since(now);
            if delay.is_zero() {
                self.meters
                    .pacing_observation(next.available(now), Duration::ZERO, false);
            } else {
                let mut wait = PacingWait::new(self.meters.as_ref(), &mut self.drift, delay);
                tokio::time::sleep_until(due).await;
                wait.complete(next.available(Instant::now()));
            }
            self.ceiling = Some(next);
        } else {
            self.meters
                .pacing_observation(Duration::ZERO, Duration::ZERO, false);
        }
        // Only admitted samples advance monitoring. Supervision may cancel a
        // sleep many times, but those retries must not invent media progress.
        let now = Instant::now();
        if let Some(floor) = &mut self.floor {
            floor.observe(advance, now)?;
        }
        // A floor failure disconnects before the informational drift warning.
        self.drift.observe(advance, now, events);
        self.watermark = next_watermark;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use crate::{
        admission::Pace,
        domain::SessionId,
        media::fixtures::{video_sample_at as sample, video_timeline as timeline},
        observe::{EventObserver, Events, ProcessMeters, SessionMeters},
    };

    use super::*;

    /// Collects what the pacer reported, so drift assertions read what an
    /// operator would see rather than internal state.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<SessionEvent>>);

    impl EventObserver for Recorder {
        fn observe(&self, _session: SessionId, event: SessionEvent) {
            self.0.lock().push(event);
        }
    }

    fn recorder() -> (Arc<Recorder>, EventSink) {
        let recorder = Arc::new(Recorder::default());
        let sink = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>)
            .scoped(SessionId(nz::u64!(1)));
        (recorder, sink)
    }

    /// An outlet for tests that are not about what was reported.
    fn discard() -> EventSink {
        Events::default().scoped(SessionId(nz::u64!(1)))
    }

    fn ceiling(burst_secs: u64) -> Ceiling {
        Ceiling {
            pace: Pace::realtime(),
            burst: Duration::from_secs(burst_secs),
        }
    }

    fn pacer(ceiling: Option<Ceiling>, floor: Option<Floor>, meters: &SessionMeters) -> MediaPacer {
        MediaPacer::after_preroll(
            ceiling,
            floor,
            &timeline(),
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the watermark")
    }

    fn meters() -> SessionMeters {
        SessionMeters::new(ProcessMeters::default())
    }

    #[tokio::test(start_paused = true)]
    async fn media_within_the_burst_is_admitted_without_waiting() {
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(Some(ceiling(10)), None, &meters);
        let started = Instant::now();

        pacer
            .pace(&sample(3), &events)
            .await
            .expect("three seconds fit");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::ZERO
        );
    }

    #[tokio::test(start_paused = true)]
    async fn media_beyond_the_burst_waits_for_the_shortfall() {
        let meters = meters();
        let events = discard();
        // Two seconds of burst against a three-second advance: one second is
        // unearned and has to be waited for at 1x.
        let mut pacer = pacer(Some(ceiling(2)), None, &meters);
        let started = Instant::now();

        pacer
            .pace(&sample(3), &events)
            .await
            .expect("the shortfall is slept");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_burst_still_paces_at_realtime() {
        // Burst is a head start, not permission to run at `pace`. An empty
        // clock schedules each advance, so a two-second jump still lands at
        // 1x rather than being refused or needing a one-second burst.
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(Some(ceiling(0)), None, &meters);
        let started = Instant::now();

        pacer
            .pace(&sample(2), &events)
            .await
            .expect("unearned media is slept, not refused");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_publisher_cannot_bank_unlimited_credit_and_burst() {
        // The bug the standing lead allowance had: its anchor was fixed at
        // pre-roll, so running slow accrued credit without bound and a
        // publisher could then replay a backlog at any speed. The clock limits
        // catch-up after the first resumed sample to `burst`.
        let meters = meters();
        let events = discard();
        let mut pacer = MediaPacer::after_preroll(
            Some(ceiling(5)),
            None,
            &timeline(),
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the watermark");

        // Sit idle far longer than the burst, advancing no media at all.
        tokio::time::advance(Duration::from_mins(10)).await;

        // The first overdue sample resumes immediately. Subsequent samples
        // can use the configured burst, but not ten minutes of idle credit.
        pacer
            .pace(&sample(1), &events)
            .await
            .expect("resume after idle");
        let started = Instant::now();
        for second in 2..=61 {
            pacer
                .pace(&sample(second), &events)
                .await
                .expect("bounded catch-up");
        }

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(55),
            "idle time is capped at the burst rather than banked in full"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_faster_pace_earns_media_time_proportionally() {
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(
            Some(Ceiling {
                pace: Pace::new(nz::u32!(2), nz::u32!(1)),
                burst: Duration::from_secs(2),
            }),
            None,
            &meters,
        );
        let started = Instant::now();

        // Ten seconds of media, two earned by the burst; the remaining eight
        // arrive at 2x and so cost four seconds of wall clock.
        pacer
            .pace(&sample(10), &events)
            .await
            .expect("the shortfall is slept at 2x");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(4)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_absent_ceiling_never_waits() {
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(None, None, &meters);
        let started = Instant::now();

        // Advancing a few seconds at a time, well inside the jump guard: what
        // is under test is that none of it waits, not how far each step moves.
        for seconds in 1..=100 {
            pacer
                .pace(&sample(seconds), &events)
                .await
                .expect("an unthrottled publisher is never delayed");
        }

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::ZERO
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_sustaining_the_floor_is_admitted() {
        let meters = meters();
        let events = discard();
        let floor = Floor {
            pace: Pace::new(nz::u32!(1), nz::u32!(2)),
            window: Duration::from_secs(30),
        };
        let mut pacer = pacer(None, Some(floor), &meters);

        // 40s of media across 40s of wall clock is comfortably above 0.5x.
        for second in 1..=40 {
            tokio::time::advance(Duration::from_secs(1)).await;
            pacer
                .pace(&sample(second), &events)
                .await
                .expect("a realtime publisher clears a half-speed floor");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_below_the_floor_is_disconnected() {
        let meters = meters();
        let events = discard();
        let floor = Floor {
            pace: Pace::new(nz::u32!(1), nz::u32!(2)),
            window: Duration::from_secs(30),
        };
        let mut pacer = pacer(None, Some(floor), &meters);

        // One second of media for every ten of wall clock is 0.1x.
        let mut result = Ok(());
        for second in 1..=10 {
            tokio::time::advance(Duration::from_secs(10)).await;
            result = pacer.pace(&sample(second), &events).await;
            if result.is_err() {
                break;
            }
        }

        assert!(
            matches!(result, Err(PacingError::BelowFloor { .. })),
            "a publisher an order of magnitude below its floor is refused, got {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_window_is_startup_grace() {
        let meters = meters();
        let events = discard();
        let floor = Floor {
            pace: Pace::realtime(),
            window: Duration::from_secs(30),
        };
        let mut pacer = pacer(None, Some(floor), &meters);

        // Well below the floor, but inside the first window, so not yet judged.
        tokio::time::advance(Duration::from_secs(20)).await;
        pacer
            .pace(&sample(1), &events)
            .await
            .expect("a publisher is not judged before its first window elapses");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_retries_without_losing_or_double_pacing_the_sample() {
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(Some(ceiling(1)), None, &meters);
        let ahead = sample(3);

        let started = Instant::now();
        for _ in 0..3 {
            tokio::select! {
                biased;
                result = pacer.pace(&ahead, &events) => {
                    panic!("the two-second deadline completed early: {result:?}");
                }
                () = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
            assert!(meters.snapshot().publisher_backpressured);
        }
        pacer
            .pace(&ahead, &events)
            .await
            .expect("retry preserves the original deadline");
        assert_eq!(Instant::now() - started, Duration::from_secs(2));
        assert_eq!(meters.snapshot().pacing_delay, Duration::from_secs(2));
        assert_eq!(
            pacer.drift.media,
            Duration::from_secs(3),
            "retries cannot invent media progress"
        );
        assert!(!meters.snapshot().publisher_backpressured);
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_drifting_behind_realtime_is_reported_without_a_floor() {
        // The case an operator most wants to hear about and the one nothing
        // ends: no floor is set, so this publisher runs indefinitely while its
        // "live" stream falls further behind.
        let meters = meters();
        let (recorded, events) = recorder();
        let mut pacer = pacer(None, None, &meters);

        // A second of media for every four of wall clock: 0.25x.
        for second in 1..=10 {
            tokio::time::advance(Duration::from_secs(4)).await;
            pacer
                .pace(&sample(second), &events)
                .await
                .expect("drift is reported, never enforced");
        }

        assert!(
            recorded
                .0
                .lock()
                .iter()
                .any(|event| matches!(event, SessionEvent::PublisherBehindRealtime { .. })),
            "a publisher at a quarter of realtime is reported behind"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_held_at_its_ceiling_is_not_reported_behind() {
        // The regression that matters: this node is the one slowing the
        // publisher down, so reporting it as drifting would blame a publisher
        // for obeying an instruction. Realtime media against a realtime
        // ceiling sits exactly on the hysteresis boundary.
        let meters = meters();
        let (recorded, events) = recorder();
        let mut pacer = pacer(Some(ceiling(2)), None, &meters);

        for second in 1..=120 {
            pacer
                .pace(&sample(second), &events)
                .await
                .expect("a throttled publisher keeps running");
        }

        assert!(
            recorded.0.lock().is_empty(),
            "a publisher the ceiling is pacing at exactly realtime is not \
             drifting, it is complying: {:?}",
            recorded.0.lock()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_after_a_ceiling_wait_is_not_reported_as_a_slow_publisher() {
        // The live loop muxes and publishes after `pace` returns. That wall
        // time used to sit inside the drift window on top of the 1x sleep, so
        // a file held at realtime logged as 0.90x and blamed the publisher.
        let meters = meters();
        let (recorded, events) = recorder();
        let mut pacer = pacer(Some(ceiling(0)), None, &meters);

        for second in 1..=30 {
            pacer
                .pace(&sample(second), &events)
                .await
                .expect("a throttled publisher keeps running");
            tokio::time::advance(Duration::from_millis(200)).await;
        }

        assert!(
            recorded.0.lock().is_empty(),
            "mux work stacked on a ceiling wait is this node's time, not drift: {:?}",
            recorded.0.lock()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn processing_time_does_not_accumulate_as_output_drift() -> Result<(), PacingError> {
        for pace in [
            Pace::realtime(),
            Pace::new(nz::u32!(2), nz::u32!(1)),
            Pace::new(nz::u32!(1), nz::u32!(2)),
        ] {
            for burst in [Duration::ZERO, Duration::from_secs(2)] {
                let meters = meters();
                let events = discard();
                let mut pacer = pacer(Some(Ceiling { pace, burst }), None, &meters);
                let started = Instant::now();
                for second in 1_u32..=120 {
                    pacer.pace(&sample(i64::from(second)), &events).await?;
                    if second > 10 {
                        assert_eq!(
                            Instant::now() - started,
                            pace.wall_for(
                                Duration::from_secs(u64::from(second)).saturating_sub(burst)
                            ),
                            "every sample keeps its deadline after startup"
                        );
                    }
                    // Variable mux/storage work, always below one media interval.
                    tokio::time::advance(Duration::from_millis(if second % 3 == 0 {
                        200
                    } else {
                        75
                    }))
                    .await;
                }
            }
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_realtime_source_does_not_get_paced_a_second_time() -> Result<(), PacingError> {
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(Some(ceiling(0)), None, &meters);
        let started = Instant::now();
        for second in 1_u32..=120 {
            tokio::time::sleep_until(started + Duration::from_secs(u64::from(second))).await;
            pacer.pace(&sample(i64::from(second)), &events).await?;
            assert_eq!(
                Instant::now() - started,
                Duration::from_secs(u64::from(second))
            );
        }
        assert_eq!(meters.snapshot().pacing_delay, Duration::ZERO);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn an_exhausted_burst_does_not_credit_its_own_sleep() -> Result<(), PacingError> {
        let meters = meters();
        let events = discard();
        let mut pacer = pacer(Some(ceiling(2)), None, &meters);
        let started = Instant::now();
        for second in 1_u32..=120 {
            pacer.pace(&sample(i64::from(second)), &events).await?;
            assert_eq!(
                Instant::now() - started,
                Duration::from_secs(u64::from(second.saturating_sub(2)))
            );
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_is_reported_once_rather_than_every_window() {
        // Hysteresis is what makes this usable: without it a publisher
        // hovering near realtime logs on a timer forever.
        let meters = meters();
        let (recorded, events) = recorder();
        let mut pacer = pacer(None, None, &meters);

        let mut media = 0;
        // Behind for two windows, then keeping up for two.
        for _ in 0..60 {
            tokio::time::advance(Duration::from_secs(2)).await;
            media += 1;
            pacer
                .pace(&sample(media), &events)
                .await
                .expect("half speed");
        }
        for _ in 0..120 {
            tokio::time::advance(Duration::from_secs(1)).await;
            media += 1;
            pacer.pace(&sample(media), &events).await.expect("realtime");
        }

        let reported: Vec<_> = recorded.0.lock().clone();
        assert!(
            matches!(
                reported.as_slice(),
                [
                    SessionEvent::PublisherBehindRealtime { .. },
                    SessionEvent::PublisherTrackingRealtime
                ]
            ),
            "each transition is announced exactly once, got {reported:?}"
        );
    }
}
