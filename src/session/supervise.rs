use std::time::Duration;

use thiserror::Error;

use tokio::time::{Instant, MissedTickBehavior};

use crate::{observe::SessionEvent, source::InputState};

use super::{
    ExecutionError, HealthEvaluation, HealthPolicy, LiveSession, SessionContext, SessionOutcome,
    health,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisionPolicy {
    pub health: HealthPolicy,
    /// How often liveness is judged while the session runs.
    pub health_interval: Duration,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            health: HealthPolicy::default(),
            health_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SupervisionError {
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error("session was terminated as unhealthy: {0}")]
    Unhealthy(HealthEvaluation),
}

/// Runs the pipeline until the input ends, the session is stopped, or it stops
/// making progress.
///
/// Written concretely rather than behind a trait. Supervision needs the phase,
/// the meters, and the stop token — all of which belong to the session — so an
/// injected implementation could only ever be handed the same three things and
/// asked to do the same three jobs.
pub async fn supervise(
    session: &mut LiveSession,
    context: &SessionContext,
    policy: SupervisionPolicy,
) -> Result<SessionOutcome, SupervisionError> {
    let mut checks = tokio::time::interval(policy.health_interval);
    checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so the first real
    // evaluation happens one interval into the session.
    checks.tick().await;

    loop {
        if let Some(reason) = context.stop().reason() {
            return Ok(finish_stopped(context, reason));
        }

        tokio::select! {
            // Prefer stopping over starting more work when both are ready.
            biased;

            reason = context.stop().stopped() => {
                return Ok(finish_stopped(context, reason));
            }

            _ = checks.tick() => {
                let evaluation = health::evaluate(
                    context.meters(),
                    Instant::now(),
                    context.phase(),
                    policy.health,
                );
                if evaluation.is_stalled() {
                    context.meters().process().unhealthy_termination();
                    context.emit(SessionEvent::Unhealthy {
                        reason: evaluation.to_string(),
                    });
                    return Err(SupervisionError::Unhealthy(evaluation));
                }
            }

            state = session.pump(context.events()) => {
                match state? {
                    InputState::Open => {}
                    InputState::Closed => return Ok(SessionOutcome::Ended),
                    InputState::Interrupted => return Ok(SessionOutcome::Interrupted),
                }
            }
        }
    }
}

fn finish_stopped(context: &SessionContext, reason: super::StopReason) -> SessionOutcome {
    let outcome = SessionOutcome::from(reason);
    if outcome == SessionOutcome::Replaced {
        context.meters().process().session_replaced();
        context.emit(SessionEvent::Displaced {
            stream: context.stream().clone(),
        });
    }
    outcome
}
