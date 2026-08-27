use std::{error::Error, sync::Arc};

use rushls::{
    domain::{SessionId, StreamId},
    ffmpeg_versions,
    hooks::{self, HookObserver},
    observe::{EventObserver, Events, NodeEvent, SessionEnd, SessionEvent, StreamEvent},
    server::{AppConfig, Node, ResolvedHooks},
    version,
};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

struct TracingEvents;

impl EventObserver for TracingEvents {
    // Keeping this exhaustive mapping together makes every operator-facing
    // session event and its severity easy to review in one place.
    #[allow(clippy::too_many_lines)]
    fn observe(&self, session: SessionId, event: SessionEvent) {
        match event {
            SessionEvent::Accepted { stream, principal } => {
                info!(session = %session, stream = %stream, principal = %principal, "accepted");
            }
            SessionEvent::Displaced { stream } => {
                info!(session = %session, stream = %stream, "displaced");
            }
            SessionEvent::TracksDiscovered { counts } => {
                info!(
                    session = %session,
                    audio = counts.audio,
                    subtitle = counts.subtitle,
                    video = counts.video,
                    "tracks discovered"
                );
            }
            SessionEvent::TimelineCalibrated { authority } => {
                info!(session = %session, authority = %authority, "timeline calibrated");
            }
            SessionEvent::SegmentationLocked { segment, part } => {
                info!(session = %session, segment = ?segment, part = ?part, "segmentation locked");
            }
            SessionEvent::SegmentationExtended {
                track,
                planned,
                actual,
            } => {
                debug!(
                    session = %session,
                    track = %track,
                    planned = ?planned,
                    actual = ?actual,
                    "segmentation extended"
                );
            }
            SessionEvent::SubtitleCueTooLate { track, late_by } => {
                warn!(
                    session = %session,
                    track = %track,
                    late_by = ?late_by,
                    "subtitle cue too late"
                );
            }
            SessionEvent::SubtitleStateLongLived {
                track,
                started_at,
                age,
            } => {
                debug!(
                    session = %session,
                    track = %track,
                    started_at = ?started_at,
                    age = ?age,
                    "subtitle state long lived"
                );
            }
            SessionEvent::ClosedCaptionsDetected { channels } => {
                info!(session = %session, channels = ?channels, "closed captions detected");
            }
            SessionEvent::ClosedCaptionsPartial {
                carrying,
                video_tracks,
            } => {
                warn!(
                    session = %session,
                    carrying,
                    video_tracks,
                    "closed captions missing from some video tracks"
                );
            }
            SessionEvent::ClosedCaptionsChannelMismatch => {
                warn!(session = %session, "closed caption channels differ across video tracks");
            }
            SessionEvent::ClosedCaptionsMalformedSei { messages } => {
                warn!(session = %session, messages, "closed captions SEI malformed");
            }
            SessionEvent::Running => {
                info!(session = %session, "running");
            }
            SessionEvent::TrackSetChanged => {
                warn!(session = %session, "track set changed");
            }
            SessionEvent::CodecParametersChanged { track } => {
                warn!(session = %session, track = %track, "codec parameters changed");
            }
            SessionEvent::Unhealthy { reason } => {
                warn!(session = %session, reason = %reason, "unhealthy");
            }
            SessionEvent::Draining => {
                info!(session = %session, "draining");
            }
            SessionEvent::DrainFailed { reason } => {
                warn!(session = %session, reason = %reason, "drain failed");
            }
            SessionEvent::Ended { end } => match end {
                SessionEnd::Unhealthy | SessionEnd::Failed => {
                    warn!(session = %session, end = %end, "ended");
                }
                SessionEnd::Ended
                | SessionEnd::Interrupted
                | SessionEnd::Replaced
                | SessionEnd::Cancelled => {
                    info!(session = %session, end = %end, "ended");
                }
            },
            SessionEvent::Failed { reason } => {
                warn!(session = %session, reason = %reason, "failed");
            }
        }
    }

    fn observe_stream(&self, stream: StreamId, event: StreamEvent) {
        match event {
            StreamEvent::Available => info!(stream = %stream, "playable"),
            StreamEvent::Retired => info!(stream = %stream, "no longer reachable"),
        }
    }

    fn observe_node(&self, event: NodeEvent) {
        match event {
            NodeEvent::ShuttingDown => {
                info!("shutting down");
            }
            NodeEvent::ListenerBound { protocol, address } => {
                info!(protocol = %protocol, %address, "listening");
            }
            NodeEvent::CertificateLoaded { certificate } => {
                info!(certificate = %certificate.display(), "serving certificate");
            }
            NodeEvent::CertificateRejected {
                certificate,
                reason,
            } => {
                warn!(
                    certificate = %certificate.display(),
                    reason = %reason,
                    "keeping previous certificate; rotation rejected"
                );
            }
            NodeEvent::CertificateWatchLost { reason } => {
                warn!(reason = %reason, "certificate rotations will no longer be noticed");
            }
            NodeEvent::HookEventDropped {
                hook,
                event,
                kind,
                reason,
                detail,
            } => {
                warn!(
                    hook = %hook,
                    event = %event,
                    kind = %kind,
                    reason,
                    detail = %detail,
                    "hook event dropped"
                );
            }
            NodeEvent::HookEventsAbandoned { hook, dropped } => {
                warn!(hook = %hook, dropped, "hook abandoned undelivered events at shutdown");
            }
            NodeEvent::HookDeliveryOutcomesUnknown { hook, count } => {
                warn!(
                    hook = %hook,
                    count,
                    "hook deliveries had unknown outcomes at shutdown"
                );
            }
            NodeEvent::HookEventUnrenderable { reason } => {
                error!(reason = %reason, "lifecycle event could not be rendered");
            }
            NodeEvent::PublisherHandshakeFailed { protocol, reason } => {
                warn!(protocol = %protocol, reason = %reason, "handshake rejected");
            }
            NodeEvent::PublisherSessionFailed { protocol, reason } => {
                warn!(protocol = %protocol, reason = %reason, "publishing session failed");
            }
            NodeEvent::ConnectionTaskPanicked { protocol, reason } => {
                error!(protocol = %protocol, reason = %reason, "connection task panicked");
            }
            NodeEvent::ListenerAddressUnavailable { protocol, reason } => {
                error!(
                    protocol = %protocol,
                    reason = %reason,
                    "listener bound but has no local address"
                );
            }
            NodeEvent::ListenerAcceptFailed { protocol, reason } => {
                error!(protocol = %protocol, reason = %reason, "listener could not accept");
            }
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize before configuration so startup failures also carry timestamps.
    init_tracing();
    let resolved = AppConfig::load_and_resolve().unwrap_or_else(|error| error.exit());
    // Before anything is served. A publication admitted against an older
    // FFmpeg would lose caption and multitrack ingest silently, so refusing to
    // start is more honest than accepting publishers this build cannot package
    // correctly. Reported alongside the banner so a support question about
    // missing captions can be answered from the first line of the log.
    let ffmpeg = ffmpeg_versions()?;
    info!(version = %version(), ffmpeg = %ffmpeg, "rushls started");
    for warning in &resolved.warnings {
        warn!(warning = %warning, "configuration warning");
    }

    // The base observer is what hooks themselves report through, so a failing
    // hook cannot produce events that re-enter it.
    let base: Arc<dyn EventObserver> = Arc::new(TracingEvents);
    let (observer, dispatchers, exported) = match resolved.hooks {
        Some(ResolvedHooks { config, client }) => {
            let (hooks, dispatchers) = hooks::build(config, client, Events::new(Arc::clone(&base)));
            (
                Arc::new(HookObserver::new(hooks.clone(), base)) as Arc<dyn EventObserver>,
                Some(dispatchers),
                Some(hooks),
            )
        }
        None => (base, None, None),
    };

    let mut node = Node::new(resolved.node, resolved.authenticator, Events::new(observer))?;
    if let Some(hooks) = exported {
        node = node.with_hooks(hooks);
    }

    // Dispatchers outlive the node deliberately. `serve` returns once every
    // session has ended, which is also when the last `session.ended` has been
    // queued, so draining afterwards is what gives those events their chance.
    let (stop, stopped) = watch::channel(false);
    let delivering = dispatchers.map(|dispatchers| tokio::spawn(dispatchers.run(stopped)));

    let served = node.serve(shutdown_signal()).await;

    if let Some(delivering) = delivering {
        let _ = stop.send(true);
        let _ = delivering.await;
    }
    served?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler installs");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
