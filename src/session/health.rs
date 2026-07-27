use std::time::Duration;

use derive_more::Display;
use tokio::time::Instant;

use crate::observe::SessionMeters;

use super::Phase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthPolicy {
    /// How long the input may deliver nothing before the session is failed.
    pub source_stall_timeout: Duration,
    /// How long normalization may produce nothing while input still arrives.
    pub media_stall_timeout: Duration,
    /// Multiple of the expected output cadence tolerated between publications.
    pub stalled_publication_multiplier: u32,
    /// Floor for the publication deadline, for very short output cadences.
    pub minimum_publication_stall_tolerance: Duration,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            source_stall_timeout: Duration::from_secs(5),
            media_stall_timeout: Duration::from_secs(5),
            stalled_publication_multiplier: 3,
            minimum_publication_stall_tolerance: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
pub enum HealthEvaluation {
    #[display("healthy")]
    Healthy,
    /// Before segmentation locks there is no cadence to fall behind.
    #[display("waiting for media")]
    WaitingForMedia,
    #[display("waiting for the first publication")]
    WaitingForFirstPublication,
    #[display("intentionally pacing an ahead-of-time publisher")]
    PacingPublisher,
    #[display("the input delivered nothing for {stalled_for:?}")]
    SourceStalled { stalled_for: Duration },
    #[display("normalization produced nothing for {stalled_for:?}")]
    MediaStalled { stalled_for: Duration },
    #[display("media publication is {overdue_by:?} overdue")]
    PublicationStalled { overdue_by: Duration },
}

impl HealthEvaluation {
    /// Whether this evaluation should terminate the session.
    pub fn is_stalled(self) -> bool {
        matches!(
            self,
            Self::SourceStalled { .. }
                | Self::MediaStalled { .. }
                | Self::PublicationStalled { .. }
        )
    }
}

/// Judges whether a session is still making the progress its phase implies.
///
/// Reads the same meters every stage already writes to, so liveness costs
/// nothing beyond the stores those stages were making anyway, and there is no
/// second source of truth to drift from the counters an operator sees.
pub fn evaluate(
    meters: &SessionMeters,
    now: Instant,
    phase: Phase,
    expected_publication_interval: Duration,
    policy: HealthPolicy,
) -> HealthEvaluation {
    // Pacing is deliberate lack of source and publication progress. Checking
    // this first keeps ordinary stall alarms from diagnosing backpressure as a
    // dead publisher.
    if meters.publisher_backpressured() {
        return HealthEvaluation::PacingPublisher;
    }

    // Before the first update, measure from session start: a source that never
    // delivered anything is exactly as stalled as one that stopped.
    let since_start = now.saturating_duration_since(meters.started_at());

    let source_idle = meters.source_idle_for(now).unwrap_or(since_start);
    if source_idle > policy.source_stall_timeout {
        return HealthEvaluation::SourceStalled {
            stalled_for: source_idle,
        };
    }

    let media_idle = meters.media_idle_for(now).unwrap_or(since_start);
    if media_idle > policy.media_stall_timeout {
        return HealthEvaluation::MediaStalled {
            stalled_for: media_idle,
        };
    }

    if phase.is_before_publication() {
        return HealthEvaluation::WaitingForMedia;
    }

    let Some(publication_idle) = meters.publication_idle_for(now) else {
        return HealthEvaluation::WaitingForFirstPublication;
    };
    let deadline = expected_publication_interval
        .saturating_mul(policy.stalled_publication_multiplier)
        .max(policy.minimum_publication_stall_tolerance);

    if publication_idle > deadline {
        HealthEvaluation::PublicationStalled {
            overdue_by: publication_idle - deadline,
        }
    } else {
        HealthEvaluation::Healthy
    }
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
            source_stall_timeout: Duration::from_secs(5),
            media_stall_timeout: Duration::from_secs(5),
            stalled_publication_multiplier: 2,
            minimum_publication_stall_tolerance: Duration::ZERO,
        }
    }

    /// Marks every stage as having just made progress.
    fn all_progress(meters: &SessionMeters) {
        meters.source_view().source_progress(1_024, 4, 0);
        meters.media_view().media_progress(4, 4);
        meters.delivery_view().delivery_progress(1, 0);
    }

    async fn advance(seconds: u64) {
        tokio::time::advance(Duration::from_secs(seconds)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_that_never_delivered_is_stalled_from_session_start() {
        let meters = meters();
        advance(6).await;

        assert_eq!(
            evaluate(
                &meters,
                Instant::now(),
                Phase::Discovering,
                Duration::ZERO,
                policy()
            ),
            HealthEvaluation::SourceStalled {
                stalled_for: Duration::from_secs(6),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalls_are_attributed_to_the_stage_that_stopped() {
        let meters = meters();
        advance(4).await;
        meters.source_view().source_progress(1_024, 4, 0);
        advance(3).await;

        // The input is three seconds idle and still within tolerance, but
        // normalization has produced nothing for the whole seven seconds.
        assert_eq!(
            evaluate(
                &meters,
                Instant::now(),
                Phase::Running,
                Duration::ZERO,
                policy()
            ),
            HealthEvaluation::MediaStalled {
                stalled_for: Duration::from_secs(7),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn planning_phases_wait_rather_than_expect_publication() {
        let meters = meters();
        meters.source_view().source_progress(1_024, 4, 0);
        meters.media_view().media_progress(4, 4);
        advance(1).await;

        assert_eq!(
            evaluate(
                &meters,
                Instant::now(),
                Phase::Segmenting,
                Duration::from_millis(200),
                policy(),
            ),
            HealthEvaluation::WaitingForMedia
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_session_publishing_on_cadence_is_healthy() {
        let meters = meters();
        all_progress(&meters);
        tokio::time::advance(Duration::from_millis(300)).await;

        assert_eq!(
            evaluate(
                &meters,
                Instant::now(),
                Phase::Running,
                Duration::from_millis(200),
                policy(),
            ),
            HealthEvaluation::Healthy
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_session_that_stopped_publishing_is_overdue() {
        let meters = meters();
        all_progress(&meters);
        advance(2).await;

        let evaluation = evaluate(
            &meters,
            Instant::now(),
            Phase::Running,
            Duration::from_millis(200),
            HealthPolicy {
                source_stall_timeout: Duration::from_secs(60),
                media_stall_timeout: Duration::from_secs(60),
                ..policy()
            },
        );

        // Two seconds idle against a 400ms deadline.
        assert_eq!(
            evaluation,
            HealthEvaluation::PublicationStalled {
                overdue_by: Duration::from_millis(1_600),
            }
        );
        assert!(evaluation.is_stalled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_session_before_its_first_publication_is_waiting_not_stalled() {
        let meters = meters();
        meters.source_view().source_progress(1_024, 4, 0);
        meters.media_view().media_progress(4, 4);
        advance(1).await;

        let evaluation = evaluate(
            &meters,
            Instant::now(),
            Phase::Running,
            Duration::from_millis(200),
            policy(),
        );

        assert_eq!(evaluation, HealthEvaluation::WaitingForFirstPublication);
        assert!(!evaluation.is_stalled());
    }

    #[tokio::test(start_paused = true)]
    async fn intentional_pacing_is_not_diagnosed_as_a_stall() {
        let meters = meters();
        all_progress(&meters);
        meters.media_view().pacing_observation(
            Duration::from_secs(10),
            Duration::from_secs(8),
            true,
        );
        advance(10).await;

        let evaluation = evaluate(
            &meters,
            Instant::now(),
            Phase::Running,
            Duration::from_millis(200),
            policy(),
        );

        assert_eq!(evaluation, HealthEvaluation::PacingPublisher);
        assert!(!evaluation.is_stalled());
    }
}
