//! The lifecycle of one publication, from handshake to teardown.
//!
//! This is the only module that depends on every other one, and the dependency
//! runs strictly one way: nothing below `session` knows that sessions exist.

use std::sync::Arc;

use thiserror::Error;
use tokio::time::timeout;

use crate::{
    admission::{AdmissionError, PublishGrant},
    delivery::hls::HlsError,
    media::{self, NormalizeError, PacingError, TimelineCalibrationError, ValidationError},
    mux::{FinishReason, MuxError},
    observe::{SessionEnd, SessionEvent, SessionMeters},
    segment::{self, PrerollError, PrerollRequest},
    source::{
        AcceptedPublish, PacketSource, PendingPublish, PublishRejection, SourceError,
        TransportError,
    },
};

mod context;
mod control;
mod health;
mod live;
mod phase;
mod registry;
mod services;
mod supervise;

pub use context::SessionContext;
pub use control::{StopReason, StopToken};
pub use health::{HealthEvaluation, HealthPolicy, evaluate as evaluate_health};
pub use live::{ExecutionError, LiveSession, MediaHead, MediaTail};
pub use phase::Phase;
pub use registry::{
    AtCapacity, Registration, Registry, RegistryError, SessionShared, SessionSnapshot,
};
pub use services::{Services, SessionConfig};
pub use supervise::{SupervisionError, SupervisionPolicy, supervise};

#[cfg(test)]
mod tests;

/// How a session stopped without failing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOutcome {
    /// The publisher's input ended.
    Ended,
    /// The input disappeared without a deliberate publisher close.
    Interrupted,
    /// A new publisher took the stream over.
    Replaced,
    /// An operator or a shutdown stopped it.
    Cancelled,
}

impl From<StopReason> for SessionOutcome {
    fn from(reason: StopReason) -> Self {
        match reason {
            StopReason::Cancelled => Self::Cancelled,
            StopReason::Replaced => Self::Replaced,
        }
    }
}

/// How a muxer and publisher should close out, given how the session ended.
///
/// A total function, not an independent decision. There are three enums in this
/// area and they are layers of the same fact rather than three separate ones:
/// [`StopReason`] is what an outside party asked for, [`SessionOutcome`] is how
/// the session actually finished, and [`FinishReason`] is the smaller lifecycle
/// vocabulary `mux` and `delivery` need: final, interrupted, or superseded.
/// Deriving it here keeps the mapping in one place and keeps session vocabulary
/// out of the layers below.
impl From<SessionOutcome> for FinishReason {
    fn from(outcome: SessionOutcome) -> Self {
        match outcome {
            SessionOutcome::Replaced => Self::Superseded,
            SessionOutcome::Interrupted => Self::Interrupted,
            // A cancellation is as final as a clean end of input: nothing is
            // coming to continue the stream.
            SessionOutcome::Ended | SessionOutcome::Cancelled => Self::Final,
        }
    }
}

impl From<SessionOutcome> for SessionEnd {
    fn from(outcome: SessionOutcome) -> Self {
        match outcome {
            SessionOutcome::Ended => Self::Ended,
            SessionOutcome::Interrupted => Self::Interrupted,
            SessionOutcome::Replaced => Self::Replaced,
            SessionOutcome::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Calibration(#[from] TimelineCalibrationError),
    #[error(transparent)]
    Normalize(#[from] NormalizeError),
    #[error(transparent)]
    Pacing(#[from] PacingError),
    #[error(transparent)]
    Preroll(#[from] PrerollError),
    #[error(transparent)]
    Mux(#[from] MuxError),
    #[error(transparent)]
    Delivery(#[from] HlsError),
    #[error(transparent)]
    Supervision(#[from] SupervisionError),
    #[error(transparent)]
    Capacity(#[from] AtCapacity),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("the session exceeded its time budget while {phase}")]
    TimedOut { phase: Phase },
}

/// Runs one publication for its entire life.
pub async fn run_session(
    pending: Box<dyn PendingPublish>,
    services: &Services,
    config: &SessionConfig,
) -> Result<SessionOutcome, SessionError> {
    let meters = SessionMeters::new(services.meters.clone());
    // Admission is bounded from out here rather than inside, because the thing
    // being guarded against is a peer that never finishes its handshake, and
    // nothing on the far side of that handshake is running yet to notice.
    let admitted = timeout(
        config.maximum_admission_time,
        admit(pending, services, &meters),
    )
    .await
    .map_err(|_| SessionError::TimedOut {
        phase: Phase::Accepted,
    })?;
    let AcceptedPublish { source, grant } = admitted?;
    services.meters.session_started();

    let stop = StopToken::new();
    let registration = services.sessions.register(&grant, meters, stop.clone())?;
    let context = registration.context(&services.events, stop);
    context.emit(SessionEvent::Accepted {
        stream: grant.stream_id.clone(),
        principal: grant.principal.to_string(),
    });

    let result = pipeline(&context, source, &grant, services, config).await;
    report(&context, services, &result);
    result
}

/// Authenticates the handshake, then either accepts it or turns it away at the
/// transport boundary.
async fn admit(
    pending: Box<dyn PendingPublish>,
    services: &Services,
    meters: &SessionMeters,
) -> Result<AcceptedPublish, SessionError> {
    let request = pending.publish_request()?;
    let grant = match services.authenticator.authenticate(&request).await {
        Ok(grant) => grant,
        Err(error) => {
            services.meters.publisher_rejected();
            pending.reject(PublishRejection::from(&error)).await?;
            return Err(error.into());
        }
    };

    // Checked before accepting so conflicts and capacity limits are expressed
    // in the source protocol. The registry re-checks under its write lock,
    // since this answer can go stale in between.
    if let Err(error) = services.sessions.preflight(&grant) {
        services.meters.publisher_rejected();
        let rejection = match error {
            RegistryError::AlreadyPublished { .. } => PublishRejection::AlreadyPublished,
            RegistryError::TakeoverInProgress { .. } | RegistryError::AtCapacity(_) => {
                PublishRejection::ServiceUnavailable
            }
        };
        pending.reject(rejection).await?;
        return Err(match error {
            RegistryError::AtCapacity(capacity) => capacity.into(),
            other => other.into(),
        });
    }

    Ok(pending.accept(grant, meters.source_view()).await?)
}

/// The stages an admitted publication passes through.
async fn pipeline(
    context: &SessionContext,
    mut source: Box<dyn PacketSource>,
    grant: &PublishGrant,
    services: &Services,
    config: &SessionConfig,
) -> Result<SessionOutcome, SessionError> {
    context.enter(Phase::Discovering);
    // Enforced from out here as well as passed in: a source that hangs while
    // probing cannot be relied upon to honour its own limit.
    let discovery = timeout(
        config.discovery.maximum_wall_time,
        source.discover(config.discovery),
    )
    .await
    .map_err(|_| SessionError::TimedOut {
        phase: Phase::Discovering,
    })??;

    context.enter(Phase::Validating);
    let presentation = media::validate(&discovery.tracks, &grant.policy)?;
    context.record_tracks(presentation.counts());
    context.emit(SessionEvent::TracksDiscovered {
        counts: presentation.counts(),
    });

    context.enter(Phase::Calibrating);
    let timeline = media::calibrate(&presentation)?;
    context.emit(SessionEvent::TimelineCalibrated {
        authority: timeline.timing_authority,
    });

    let mut head = MediaHead::new(
        source,
        services.normalizers.start(&presentation, &timeline)?,
        context.meters().media_view(),
        config.input,
        &timeline,
    );

    context.enter(Phase::Segmenting);
    let preroll = segment::run_preroll(
        &mut head,
        PrerollRequest {
            presentation: &presentation,
            timeline: &timeline,
            limits: config.preroll,
            policy: config.segmentation,
        },
        context.events(),
    )
    .await?;

    let pacer = media::MediaPacer::after_preroll(
        grant.policy.ingest_timing,
        &timeline,
        &preroll.buffered,
        context.meters().media_view(),
    )?;
    let started = services.muxers.start(crate::mux::MuxerStartRequest {
        presentation: &presentation,
        segmentation: &preroll.segmentation,
        // One origin is captured for the complete muxer publication. The CMAF
        // muxer remains responsible for any output edit lists needed to align
        // tracks; delivery only advances this wall time by packaged timing.
        time_anchor: std::time::SystemTime::now(),
        events: context.events(),
    })?;
    let expected_publication_interval = started.muxer.expected_publication_interval();
    let publisher = services
        .publishers
        .start(context.stream(), Arc::clone(&started.presentation))?;
    let tail = MediaTail::new(
        started.muxer,
        publisher,
        context.meters().mux_view(),
        context.meters().delivery_view(),
    );

    context.enter(Phase::Running);
    context.emit(SessionEvent::Running);
    let mut live = LiveSession::new(head, tail, pacer, preroll.buffered, preroll.input_state);
    let supervised = supervise(
        &mut live,
        context,
        config.supervision,
        expected_publication_interval,
    )
    .await;

    // A real failure decides the session's fate before the drain gets a say:
    // draining a broken pipeline is best-effort by definition, and letting it
    // overwrite the error would hide why the session actually stopped.
    let outcome = supervised?;

    context.enter(Phase::Draining);
    context.emit(SessionEvent::Draining);

    // A failed flush does not retract a session that ran. Media the publisher
    // sent was published; only the last few frames of tail are in doubt. It is
    // counted and reported so it cannot pass unnoticed, but a node that marked
    // every such session failed would report a fleet-wide fault every time an
    // encoder was cut off mid-segment.
    if let Err(error) = live.finish(outcome.into()) {
        services.meters.drain_failed();
        context.emit(SessionEvent::DrainFailed {
            reason: error.to_string(),
        });
    }
    Ok(outcome)
}

fn report(
    context: &SessionContext,
    services: &Services,
    result: &Result<SessionOutcome, SessionError>,
) {
    match result {
        Ok(outcome) => {
            services.meters.session_completed();
            context.emit(SessionEvent::Ended {
                end: (*outcome).into(),
            });
        }
        Err(error) => {
            services.meters.session_failed();
            context.emit(SessionEvent::Failed {
                reason: error.to_string(),
            });
        }
    }
}
