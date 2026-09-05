use std::{cmp::Ordering, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::time::Instant;

use crate::{
    admission::{Ceiling, Floor},
    domain::{MediaInstant, TrackId},
    observe::MediaMeters,
};

use super::{NormalizedSample, TimelineCalibration};

struct PacingWait<'a> {
    meters: &'a dyn MediaMeters,
    started: Instant,
    lead: Duration,
    completed: bool,
}

impl<'a> PacingWait<'a> {
    fn new(meters: &'a dyn MediaMeters, lead: Duration) -> Self {
        meters.pacing_observation(lead, Duration::ZERO, true);
        Self {
            meters,
            started: Instant::now(),
            lead,
            completed: false,
        }
    }

    fn complete(&mut self, media_lead: Duration) {
        let elapsed = Instant::now().saturating_duration_since(self.started);
        self.meters.pacing_observation(media_lead, elapsed, false);
        self.completed = true;
    }
}

impl Drop for PacingWait<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let elapsed = Instant::now().saturating_duration_since(self.started);
            self.meters
                .pacing_observation(self.lead.saturating_sub(elapsed), elapsed, true);
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
    sample: &NormalizedSample,
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
        sample: &NormalizedSample,
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
    #[error("normalized media jumped forward by {jump:?}, above the permitted {maximum:?}")]
    TimestampJump { maximum: Duration, jump: Duration },
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

/// Consumable permission to run ahead of wall clock.
///
/// Charged in *media time*: advancing the timeline by one second spends one
/// second, however many samples carried it. That is what lets a forward jump
/// later be treated as elapsed media rather than as free progress, and it
/// keeps the arithmetic independent of access-unit size.
///
/// Replaces a standing lead allowance, which a publisher could sit inside
/// indefinitely and which — because it was measured against an anchor fixed at
/// pre-roll — let a slow publisher accrue unbounded credit and then spend it
/// in one burst.
struct TokenBucket {
    ceiling: Ceiling,
    /// Media time available to spend without waiting.
    available: Duration,
    /// When `available` was last brought up to date.
    refilled_at: Instant,
}

impl TokenBucket {
    fn new(ceiling: Ceiling, now: Instant) -> Self {
        Self {
            ceiling,
            // Full at the start: the burst is the head start.
            available: ceiling.burst,
            refilled_at: now,
        }
    }

    /// Credits elapsed wall clock, saturating at `burst`.
    ///
    /// The cap is what stops idle time becoming savings; without it a
    /// publisher that paused could bank the whole pause and replay it at any
    /// speed afterwards.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.refilled_at);
        self.refilled_at = now;
        self.available = self
            .available
            .saturating_add(self.ceiling.pace.media_for(elapsed))
            .min(self.ceiling.burst);
    }

    /// How long to wait before `media` may be spent, and spends it.
    fn take(&mut self, media: Duration, now: Instant) -> Duration {
        self.refill(now);
        if let Some(remaining) = self.available.checked_sub(media) {
            self.available = remaining;
            return Duration::ZERO;
        }
        // Wait for exactly the shortfall, then spend the bucket down to empty:
        // the wait is what earns the difference.
        let shortfall = media.saturating_sub(self.available);
        self.available = Duration::ZERO;
        self.refilled_at = now;
        self.ceiling.pace.wall_for(shortfall)
    }
}

/// Whether a publisher is sustaining the minimum rate its policy requires.
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

/// Maps normalized media time onto a monotonic wall clock without rewriting it.
pub struct MediaPacer {
    /// Absent means unthrottled: media is admitted as fast as it arrives.
    bucket: Option<TokenBucket>,
    /// Absent means no minimum rate is required.
    floor: Option<FloorWindow>,
    maximum_timestamp_jump: Duration,
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
        maximum_timestamp_jump: Duration,
        timeline: &TimelineCalibration,
        buffered: &[NormalizedSample],
        meters: Arc<dyn MediaMeters>,
    ) -> Result<Self, PacingError> {
        let mut watermark = MediaWatermark::default();
        for sample in buffered {
            watermark.observe(sample, timeline)?;
        }
        let now = Instant::now();
        Ok(Self {
            bucket: ceiling.map(|ceiling| TokenBucket::new(ceiling, now)),
            floor: floor.map(|floor| FloorWindow::new(floor, now)),
            maximum_timestamp_jump,
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
    pub async fn pace(&mut self, sample: &NormalizedSample) -> Result<(), PacingError> {
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
        // nothing, which is what makes the bucket's initial fill the head
        // start rather than a second one.
        let advance = match previous {
            Some(previous) => current.elapsed_since(previous)?,
            None => Duration::ZERO,
        };

        // Checked before anything is charged, and independently of whether a
        // ceiling exists: a forward jump inflates the timeline whether or not
        // anyone is pacing it, and charging one to the bucket would sleep for
        // what is probably a broken clock.
        if advance > self.maximum_timestamp_jump {
            return Err(PacingError::TimestampJump {
                maximum: self.maximum_timestamp_jump,
                jump: advance,
            });
        }

        let now = Instant::now();
        if let Some(floor) = &mut self.floor {
            floor.observe(advance, now)?;
        }

        match &mut self.bucket {
            None => {
                self.meters
                    .pacing_observation(Duration::ZERO, Duration::ZERO, false);
            }
            Some(bucket) => {
                let delay = bucket.take(advance, now);
                if delay.is_zero() {
                    self.meters
                        .pacing_observation(bucket.available, Duration::ZERO, false);
                } else {
                    let mut wait = PacingWait::new(self.meters.as_ref(), delay);
                    tokio::time::sleep(delay).await;
                    wait.complete(Duration::ZERO);
                }
            }
        }
        self.watermark = next_watermark;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::Pace,
        media::fixtures::{video_sample_at as sample, video_timeline as timeline},
        observe::{ProcessMeters, SessionMeters},
    };

    use super::*;

    fn ceiling(burst_secs: u64) -> Ceiling {
        Ceiling {
            pace: Pace::realtime(),
            burst: Duration::from_secs(burst_secs),
        }
    }

    fn pacer(
        ceiling: Option<Ceiling>,
        floor: Option<Floor>,
        meters: &SessionMeters,
    ) -> MediaPacer {
        MediaPacer::after_preroll(
            ceiling,
            floor,
            Duration::from_secs(10),
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
        let mut pacer = pacer(Some(ceiling(10)), None, &meters);
        let started = Instant::now();

        pacer.pace(&sample(3)).await.expect("three seconds fit");

        assert_eq!(Instant::now().saturating_duration_since(started), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn media_beyond_the_burst_waits_for_the_shortfall() {
        let meters = meters();
        // Two seconds of burst against a three-second advance: one second is
        // unearned and has to be waited for at 1x.
        let mut pacer = pacer(Some(ceiling(2)), None, &meters);
        let started = Instant::now();

        pacer.pace(&sample(3)).await.expect("the shortfall is slept");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_publisher_cannot_bank_unlimited_credit_and_burst() {
        // The bug the standing lead allowance had: its anchor was fixed at
        // pre-roll, so running slow accrued credit without bound and a
        // publisher could then replay a backlog at any speed. The bucket caps
        // what idling is worth at exactly `burst`.
        let meters = meters();
        let mut pacer = MediaPacer::after_preroll(
            Some(ceiling(5)),
            None,
            // Generous, so the jump guard plays no part in what is under test.
            Duration::from_mins(10),
            &timeline(),
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the watermark");

        // Sit idle far longer than the burst, advancing no media at all.
        tokio::time::advance(Duration::from_mins(10)).await;

        let started = Instant::now();
        // A minute of media offered at once: only the five-second burst is
        // available, so the remaining 55s must be earned in real time.
        pacer.pace(&sample(60)).await.expect("the excess is slept");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(55),
            "idle time is capped at the burst rather than banked in full"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_faster_pace_earns_media_time_proportionally() {
        let meters = meters();
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
        pacer.pace(&sample(10)).await.expect("the shortfall is slept at 2x");

        assert_eq!(
            Instant::now().saturating_duration_since(started),
            Duration::from_secs(4)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_absent_ceiling_never_waits() {
        let meters = meters();
        let mut pacer = pacer(None, None, &meters);
        let started = Instant::now();

        // Advancing a few seconds at a time, well inside the jump guard: what
        // is under test is that none of it waits, not how far each step moves.
        for seconds in 1..=100 {
            pacer
                .pace(&sample(seconds))
                .await
                .expect("an unthrottled publisher is never delayed");
        }

        assert_eq!(Instant::now().saturating_duration_since(started), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forward_jump_is_refused_even_without_a_ceiling() {
        let meters = meters();
        let mut pacer = MediaPacer::after_preroll(
            None,
            None,
            Duration::from_secs(1),
            &timeline(),
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the watermark");

        assert_eq!(
            pacer.pace(&sample(3)).await,
            Err(PacingError::TimestampJump {
                maximum: Duration::from_secs(1),
                jump: Duration::from_secs(3),
            }),
            "the jump guard is independent of pacing, which is the point of \
             detaching it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_sustaining_the_floor_is_admitted() {
        let meters = meters();
        let floor = Floor {
            pace: Pace::new(nz::u32!(1), nz::u32!(2)),
            window: Duration::from_secs(30),
        };
        let mut pacer = pacer(None, Some(floor), &meters);

        // 40s of media across 40s of wall clock is comfortably above 0.5x.
        for second in 1..=40 {
            tokio::time::advance(Duration::from_secs(1)).await;
            pacer
                .pace(&sample(second))
                .await
                .expect("a realtime publisher clears a half-speed floor");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_below_the_floor_is_disconnected() {
        let meters = meters();
        let floor = Floor {
            pace: Pace::new(nz::u32!(1), nz::u32!(2)),
            window: Duration::from_secs(30),
        };
        let mut pacer = pacer(None, Some(floor), &meters);

        // One second of media for every ten of wall clock is 0.1x.
        let mut result = Ok(());
        for second in 1..=10 {
            tokio::time::advance(Duration::from_secs(10)).await;
            result = pacer.pace(&sample(second)).await;
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
        let floor = Floor {
            pace: Pace::realtime(),
            window: Duration::from_secs(30),
        };
        let mut pacer = pacer(None, Some(floor), &meters);

        // Well below the floor, but inside the first window, so not yet judged.
        tokio::time::advance(Duration::from_secs(20)).await;
        pacer
            .pace(&sample(1))
            .await
            .expect("a publisher is not judged before its first window elapses");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_retries_without_losing_or_double_pacing_the_sample() {
        let meters = meters();
        let mut pacer = pacer(Some(ceiling(1)), None, &meters);
        let ahead = sample(3);

        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            result = pacer.pace(&ahead) => {
                panic!("the two-second pacing wait completed early: {result:?}");
            }
        }

        assert!(meters.snapshot().publisher_backpressured);

        pacer
            .pace(&ahead)
            .await
            .expect("the retry waits only for the remaining shortfall");
        assert!(!meters.snapshot().publisher_backpressured);
    }
}
