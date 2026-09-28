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
mod pending;
mod phase;
mod registry;
mod services;
mod supervise;

pub use context::SessionContext;
pub use control::{StopReason, StopToken};
pub use health::{HealthEvaluation, HealthPolicy, evaluate as evaluate_health};
pub use live::{ExecutionError, LiveSession, MediaHead, MediaTail};
pub use pending::{PendingPermit, PendingPublishers};
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
///
/// `slot` is the caller's reservation from [`PendingPublishers`], released as
/// soon as admission concludes: everything past that point is bounded by the
/// registry's session capacity instead.
pub async fn run_session(
    pending: Box<dyn PendingPublish>,
    services: &Services,
    config: &SessionConfig,
    slot: PendingPermit,
) -> Result<SessionOutcome, SessionError> {
    let meters = SessionMeters::with_budget(
        services.meters.clone(),
        crate::domain::PipelineBudget::for_publisher(config.memory_per_publisher),
    );
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
    });
    // Explicit rather than left to scope, because *when* this is released is
    // the whole point: a session that runs for hours must not still be holding
    // a slot that exists to absorb connection bursts.
    drop(slot);
    let SessionAdmission {
        source,
        grant,
        publisher,
    } = admitted??;
    services.meters.session_started();

    let stop = StopToken::new();
    let registration = services.sessions.register(&grant, meters, stop.clone())?;
    let context = registration.context(&services.events, stop);
    context.emit(SessionEvent::Accepted {
        stream: grant.stream_id.clone(),
        principal: grant.principal.to_string(),
        publisher,
    });

    let result = pipeline(&context, source, &grant, services, config).await;
    report(&context, services, &result);
    result
}

// What admission hands to the session: transport output plus the observed
// publisher identity preserved from the request.
struct SessionAdmission {
    source: Box<dyn PacketSource>,
    grant: PublishGrant,
    publisher: crate::domain::PublisherContext,
}

/// Authenticates the handshake, then either accepts it or turns it away at the
/// transport boundary.
async fn admit(
    pending: Box<dyn PendingPublish>,
    services: &Services,
    meters: &SessionMeters,
) -> Result<SessionAdmission, SessionError> {
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
            RegistryError::TakeoverInProgress { .. }
            | RegistryError::AtCapacity(_)
            | RegistryError::MemoryFull(_) => PublishRejection::ServiceUnavailable,
        };
        pending.reject(rejection).await?;
        return Err(match error {
            RegistryError::AtCapacity(capacity) => capacity.into(),
            other => other.into(),
        });
    }

    let publisher = crate::domain::PublisherContext {
        protocol: request.protocol,
        resource: request.resource,
        client: request.client,
    };
    let AcceptedPublish { source, grant } = pending.accept(grant, meters.source_view()).await?;
    Ok(SessionAdmission {
        source,
        grant,
        publisher,
    })
}

/// The stages an admitted publication passes through.
fn start_normalization(
    context: &SessionContext,
    presentation: &crate::media::PresentationPlan,
    grant: &PublishGrant,
    services: &Services,
) -> Result<crate::media::StartedNormalizer, SessionError> {
    context.enter(Phase::Calibrating);
    let timeline = media::calibrate(presentation)?;
    context.emit(SessionEvent::TimelineCalibrated {
        authority: timeline.timing_authority,
    });
    let normalized =
        services
            .normalizers
            .start(presentation, &timeline, grant.policy.input_mode)?;
    record_track_timing(context, &normalized.presentation, &normalized.timeline);

    Ok(normalized)
}

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

    let normalized = start_normalization(context, &presentation, grant, services)?;
    let presentation = normalized.presentation;
    let timeline = normalized.timeline;

    let mut head = MediaHead::new(
        source,
        normalized.normalizer,
        context.meters().media_view(),
        config.input,
        &timeline,
        // Captions are declared from what the bitstream actually carries, so
        // the language advertised comes from the video track that carries them
        // rather than from a separate declaration that could disagree.
        Some(media::CaptionVerifier::new(
            presentation.tracks(),
            video_language(&presentation),
        )),
    );

    head.set_events(context.events().clone());

    context.enter(Phase::Segmenting);
    let preroll = segment::run_preroll(
        &mut head,
        PrerollRequest {
            presentation: &presentation,
            timeline: &timeline,
            limits: config.preroll,
            policy: config.segmentation,
            budget: context.meters().budget(),
        },
        context.events(),
    )
    .await?;

    let pacer = media::MediaPacer::after_preroll(
        grant.policy.ceiling,
        grant.policy.floor,
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
        budget: context.meters().budget(),
    })?;
    context.emit(SessionEvent::SegmentationContract {
        desired_segment: config.segmentation.desired_segment_duration,
        desired_part: config.segmentation.desired_part_duration,
        selected_segment: preroll.segmentation.longest_segment_duration(),
        selected_part: preroll
            .segmentation
            .iter()
            .map(|track| track.timebase.ticks_to_duration(track.part_duration.get()))
            .max()
            .unwrap_or_default(),
        maximum_segment: config.segmentation.maximum_segment_duration,
        maximum_part: config.segmentation.maximum_part_duration,
        jitter: config.segmentation.late_boundary,
    });
    let publisher = services
        .publishers
        .start(context.stream(), Arc::clone(&started.presentation))?;
    let tail = MediaTail::new(
        started.muxer,
        publisher,
        context.meters().mux_view(),
        context.meters().delivery_view(),
    )
    .with_input_mode(grant.policy.input_mode);

    context.enter(Phase::Running);
    context.emit(SessionEvent::Running);
    let mut live = LiveSession::new(head, tail, pacer, preroll.buffered, preroll.input_state);
    let supervised = supervise(&mut live, context, config.supervision).await;

    // A real failure decides the session's fate before the drain gets a say:
    // draining a broken pipeline is best-effort by definition, and letting it
    // overwrite the error would hide why the session actually stopped.
    let outcome = supervised?;

    context.enter(Phase::Draining);
    context.emit(SessionEvent::Draining);

    finish_live(&mut live, outcome, context, services)?;
    Ok(outcome)
}

fn finish_live(
    live: &mut LiveSession,
    outcome: SessionOutcome,
    context: &SessionContext,
    services: &Services,
) -> Result<(), SessionError> {
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
        // A typed input timing failure is still a publisher fault when only
        // final flushing exposes it. Preserve best-effort drain for other errors.
        if let live::ExecutionError::Media(media::MediaError::Normalize(
            error @ NormalizeError::Timestamp(_),
        )) = error
        {
            return Err(SessionError::Normalize(error));
        }
    }
    Ok(())
}

fn record_track_timing(
    context: &SessionContext,
    presentation: &media::PresentationPlan,
    timeline: &media::TimelineCalibration,
) {
    context
        .meters()
        .tracks()
        .register(presentation.tracks().iter().filter_map(|track| {
            timeline
                .get(track.id)
                .map(|clock| (track.id, track.kind(), clock.timebase, clock.origin_pts))
        }));
}

/// The language a caption service should advertise.
///
/// In-band captions carry no language tag of their own — the SEI has no field
/// for one — so the video track carrying them is the only evidence available.
/// `LANGUAGE` is optional on `EXT-X-MEDIA`, so its absence withholds the
/// attribute rather than the whole declaration.
fn video_language(presentation: &media::PresentationPlan) -> Option<Arc<str>> {
    presentation
        .tracks()
        .iter()
        .find(|track| track.kind() == crate::domain::MediaKind::Video)
        .and_then(|track| track.language.as_deref())
        .map(Arc::from)
}

fn timestamp_issue(error: &SessionError) -> Option<&crate::domain::TimestampIssue> {
    let (SessionError::Normalize(normalized)
    | SessionError::Preroll(PrerollError::Media(media::MediaError::Normalize(normalized)))
    | SessionError::Supervision(SupervisionError::Execution(live::ExecutionError::Media(
        media::MediaError::Normalize(normalized),
    )))) = error
    else {
        return None;
    };
    match normalized {
        NormalizeError::Timestamp(issue) => Some(issue),
        _ => None,
    }
}

fn report(
    context: &SessionContext,
    services: &Services,
    result: &Result<SessionOutcome, SessionError>,
) {
    services
        .meters
        .pipeline_exhaustions(context.meters().snapshot().pipeline_failures);
    match result {
        Ok(outcome) => {
            services.meters.session_completed();
            context.emit(SessionEvent::Ended {
                end: (*outcome).into(),
            });
        }
        Err(error) => {
            services.meters.session_failed();
            let mux = match error {
                SessionError::Mux(error)
                | SessionError::Preroll(PrerollError::Packaging(error))
                | SessionError::Supervision(SupervisionError::Execution(
                    live::ExecutionError::Mux(error),
                )) => Some(error),
                _ => None,
            };
            if let Some(error) = mux {
                services.meters.segmentation_failed(error);
            }
            let timestamp_issue = timestamp_issue(error).cloned().map(Box::new);
            if let Some(issue) = &timestamp_issue {
                services.meters.timestamp_rejected(issue);
            }
            context.emit(SessionEvent::Failed {
                timestamp_issue,
                segmentation: mux.cloned(),
                reason: error.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod gap_playback;
