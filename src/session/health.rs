use std::time::Duration;

use derive_more::Display;
use tokio::time::Instant;

use crate::observe::SessionMeters;

use super::Phase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthPolicy {
    /// How long a publisher may deliver nothing usable before it is dropped.
    ///
    /// One deadline rather than one per stage. Three timers were three chances
    /// to fail a healthy publisher, and which stage noticed first is a
    /// diagnosis — reported on the event below — rather than something an
    /// operator should have to size separately.
    pub stall: Duration,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(12),
        }
    }
}

/// Which stage last showed life, when a session is dropped for stalling.
///
/// Not a policy input: both cases end the session identically. It exists
/// because "stalled" alone sends an operator to the wrong layer — a silent
/// peer and a peer still sending bytes that never become media are different
/// faults with the same symptom.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
pub enum StallSource {
    /// Nothing arrived from the publisher at all.
    #[display("the publisher sent nothing")]
    Publisher,
    /// Bytes kept arriving but none of them became media.
    #[display("the publisher's media could not be decoded")]
    Media,
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
pub enum HealthEvaluation {
    #[display("healthy")]
    Healthy,
    /// Before segmentation locks there is no cadence to fall behind.
    #[display("waiting for media")]
    WaitingForMedia,
    #[display("nothing usable arrived for {stalled_for:?} ({source})")]
    Stalled {
        stalled_for: Duration,
        source: StallSource,
    },
}

impl HealthEvaluation {
    /// Whether this evaluation should terminate the session.
    pub fn is_stalled(self) -> bool {
        matches!(self, Self::Stalled { .. })
    }
}

/// Judges whether a session is still delivering usable media.
///
/// Reads the same meters every stage already writes to, so liveness costs
/// nothing beyond the stores those stages were making anyway, and there is no
/// second source of truth to drift from the counters an operator sees.
///
/// Deliberately knows nothing about wall-clock cadence. How fast media advances
/// against wall time is what `ceiling` and `floor` answer; this asks only
/// whether anything is arriving at all. The two were previously entangled in a
/// publication deadline that dropped any publisher slower than its own segment
/// cadence — including one a ceiling was deliberately throttling.
pub fn evaluate(
    meters: &SessionMeters,
    now: Instant,
    phase: Phase,
    policy: HealthPolicy,
) -> HealthEvaluation {
    // Before the first update, measure from session start: a source that never
    // delivered anything is exactly as stalled as one that stopped.
    //
    // Pacing sleeps are already excluded by the meters, so a throttled
    // publisher does not age these clocks while it waits. Suppressing the
    // checks outright — the previous approach — left a swallowing muxer with no
    // alarm that could see it for as long as the pacer slept.
    let since_start = now.saturating_duration_since(meters.started_at());
    let media_idle = meters.media_idle_for(now).unwrap_or(since_start);
    if media_idle > policy.stall {
        // Attribution, not a second deadline: the publisher is dropped either
        // way, and this only decides which layer the operator is pointed at.
        let source = if meters.source_idle_for(now).unwrap_or(since_start) > policy.stall {
            StallSource::Publisher
        } else {
            StallSource::Media
        };
        return HealthEvaluation::Stalled {
            stalled_for: media_idle,
            source,
        };
    }

    if phase.is_before_publication() {
        return HealthEvaluation::WaitingForMedia;
    }

    HealthEvaluation::Healthy
}

#[cfg(test)]
mod tests {
    use crate::observe::ProcessMeters;

    use super::*;

    fn meters() -> SessionMeters {
        SessionMeters::new(ProcessMeters::default())
    }

    fn policy() -> HealthPolicy {
        HealthPolicy {
            stall: Duration::from_secs(5),
        }
    }

    async fn advance(seconds: u64) {
        tokio::time::advance(Duration::from_secs(seconds)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_publisher_that_never_delivered_is_stalled_from_session_start() {
        let meters = meters();
        advance(6).await;

        assert_eq!(
            evaluate(&meters, Instant::now(), Phase::Discovering, policy()),
            HealthEvaluation::Stalled {
                stalled_for: Duration::from_secs(6),
                source: StallSource::Publisher,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stall_is_attributed_to_the_stage_that_stopped() {
        let meters = meters();
        advance(4).await;
        // Bytes are still arriving; none of them are becoming media.
        meters.source_view().source_progress(1_024, 4);
        advance(3).await;

        assert_eq!(
            evaluate(&meters, Instant::now(), Phase::Running, policy()),
            HealthEvaluation::Stalled {
                stalled_for: Duration::from_secs(7),
                source: StallSource::Media,
            },
            "a peer still sending unusable bytes is a different fault from a \
             silent one, and an operator told the wrong one looks in the wrong \
             layer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn planning_phases_wait_rather_than_expect_publication() {
        let meters = meters();
        meters.source_view().source_progress(1_024, 4);
        meters.media_view().media_progress(4, 4);
        advance(1).await;

        assert_eq!(
            evaluate(&meters, Instant::now(), Phase::Segmenting, policy()),
            HealthEvaluation::WaitingForMedia
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_session_still_producing_media_is_healthy() {
        let meters = meters();
        meters.source_view().source_progress(1_024, 4);
        meters.media_view().media_progress(4, 4);
        tokio::time::advance(Duration::from_millis(300)).await;

        assert_eq!(
            evaluate(&meters, Instant::now(), Phase::Running, policy()),
            HealthEvaluation::Healthy
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_publisher_is_never_unhealthy_for_being_slow() {
        // The regression this whole rework exists for. A publisher delivering
        // far less media than wall time is exactly what a ceiling produces, and
        // what an operator without a floor has said is acceptable. Health must
        // not have an opinion.
        let meters = meters();
        let policy = HealthPolicy {
            stall: Duration::from_secs(12),
        };

        for _ in 0..20 {
            advance(10).await;
            meters.source_view().source_progress(1_024, 1);
            meters.media_view().media_progress(1, 1);
            assert_eq!(
                evaluate(&meters, Instant::now(), Phase::Running, policy),
                HealthEvaluation::Healthy
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_swallowing_muxer_is_still_caught_while_the_publisher_is_throttled() {
        // Pacing used to suppress this check outright, which left a muxer that
        // consumed every sample and emitted nothing undetectable for as long as
        // the pacer slept. The clocks now exclude pacing sleeps instead, so the
        // alarm stays armed: media stops advancing and that is visible.
        let meters = meters();
        meters.source_view().source_progress(1_024, 4);
        meters.media_view().media_progress(4, 4);
        meters
            .media_view()
            .pacing_observation(Duration::ZERO, Duration::ZERO, true);
        advance(6).await;

        assert!(
            evaluate(&meters, Instant::now(), Phase::Running, policy()).is_stalled(),
            "backpressure must not shield a stage that stopped producing"
        );
    }
}
