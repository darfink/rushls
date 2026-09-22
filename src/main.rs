use std::{error::Error, sync::Arc};

use rushls::{
    domain::{SessionId, StreamId},
    hooks::{self, HookObserver},
    observe::{EventObserver, Events, NodeEvent, SessionEnd, SessionEvent, StreamEvent},
    server::{AppConfig, Node, ResolvedHooks},
    version,
};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

// Instrumentation is absent from ordinary production builds.
#[cfg(feature = "allocation-counting")]
#[global_allocator]
static ALLOCATOR: rushls::allocation::CountingAllocator = rushls::allocation::CountingAllocator;

struct TracingEvents;

impl EventObserver for TracingEvents {
    // Keeping this exhaustive mapping together makes every operator-facing
    // session event and its severity easy to review in one place.
    #[allow(clippy::too_many_lines)]
    fn observe(&self, session: SessionId, event: SessionEvent) {
        match event {
            SessionEvent::Accepted {
                stream,
                principal,
                publisher,
            } => {
                info!(session = %session, stream = %stream, principal = %principal, protocol = %publisher.protocol, remote_address = %publisher.client.remote_address, "accepted");
            }
            SessionEvent::Displaced { stream } => {
                info!(session = %session, stream = %stream, "displaced");
            }
            SessionEvent::TracksDiscovered { counts } => {
                debug!(
                    session = %session,
                    audio = counts.audio,
                    subtitle = counts.subtitle,
                    video = counts.video,
                    "tracks discovered"
                );
            }
            SessionEvent::TimelineCalibrated { authority } => {
                debug!(session = %session, authority = %authority, "timeline calibrated");
            }
            SessionEvent::SegmentationLocked { segment, part } => {
                debug!(session = %session, segment = ?segment, part = ?part, "segmentation locked");
            }
            SessionEvent::SegmentationContract {
                desired_segment,
                desired_part,
                selected_segment,
                selected_part,
                maximum_segment,
                maximum_part,
                jitter,
            } => {
                debug!(session = %session, ?desired_segment, ?desired_part, ?selected_segment, ?selected_part, ?maximum_segment, ?maximum_part, ?jitter, "segmentation contract");
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
                debug!(session = %session, "running");
            }
            SessionEvent::TrackSetChanged => {
                warn!(session = %session, "track set changed");
            }
            SessionEvent::CodecParametersChanged { track } => {
                warn!(session = %session, track = %track, "codec parameters changed");
            }
            SessionEvent::PublisherBehindRealtime { window, media } => {
                let pace = if window.is_zero() {
                    0.0
                } else {
                    media.as_secs_f64() / window.as_secs_f64()
                };
                warn!(
                    session = %session,
                    window = ?window,
                    media = ?media,
                    pace = %format!("{pace:.2}x"),
                    "publisher is behind realtime"
                );
            }
            SessionEvent::PublisherTrackingRealtime => {
                info!(session = %session, "publisher is tracking realtime again");
            }
            SessionEvent::Unhealthy { reason } => {
                warn!(session = %session, reason = %reason, "unhealthy");
            }
            SessionEvent::Draining => {
                debug!(session = %session, "draining");
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
            SessionEvent::Compensation { notice } => match notice.transition {
                rushls::domain::RecoveryTransition::Unavailable => {
                    warn!(session = %session, status = ?notice.status, "video cadence validation unavailable");
                }
                rushls::domain::RecoveryTransition::Degraded => {
                    warn!(session = %session, track = %notice.status.track, media_kind = ?notice.status.media_kind, codec = ?notice.status.codec, method = %notice.status.method, missing_ticks = %notice.status.missing_ticks, episode_ticks = %notice.status.episode_ticks, total_ticks = %notice.status.total_ticks, timebase = ?notice.status.timebase, cadence = ?notice.status.cadence, "input compensation started");
                }
                rushls::domain::RecoveryTransition::Recovered => {
                    info!(session = %session, status = ?notice.status, "input continuity recovered");
                }
                rushls::domain::RecoveryTransition::Compensated => {}
            },
            SessionEvent::Failed {
                reason,
                segmentation,
                timestamp_issue,
            } => {
                warn!(session = %session, reason = %reason, ?segmentation, ?timestamp_issue, "failed");
            }
        }
    }

    fn observe_stream(&self, stream: StreamId, event: StreamEvent) {
        match event {
            StreamEvent::SegmentReady(segment) => {
                debug!(stream = %stream, rendition = segment.rendition_id, segment = segment.segment_id, "segment ready");
            }
            StreamEvent::Available => info!(stream = %stream, "playable"),
            StreamEvent::Retired => info!(stream = %stream, "no longer reachable"),
            StreamEvent::RetentionClipped {
                reason,
                requested,
                held,
            } => {
                warn!(
                    stream = %stream,
                    capacity = %reason,
                    retain = %humantime::format_duration(requested),
                    retained = %humantime::format_duration(held),
                    "storage capacity reached; oldest media dropped"
                );
            }
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
            NodeEvent::RecordingFailed { stream, reason } => {
                error!(%stream, %reason, "recording failed");
            }
            NodeEvent::RecordingRecovered { lost } => {
                info!(lost, "recording recovered");
            }
            NodeEvent::RecordingDrainExpired { pending } => {
                warn!(
                    pending,
                    "recording shutdown deadline expired; pending outcomes unknown"
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
    let timestamp_format = time::format_description::parse_owned::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z",
    )
    .expect("the log timestamp format is valid");
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_timer(fmt::time::UtcTime::new(timestamp_format))
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize before configuration so startup failures also carry timestamps.
    init_tracing();
    let resolved = AppConfig::load_and_resolve().unwrap_or_else(|error| error.exit());
    if let Some(path) = &resolved.config_file {
        info!(path = %path.display(), "loaded configuration");
    } else {
        info!("no configuration file; using compiled defaults");
    }
    info!(version = %version(), "rushls started");
    for warning in &resolved.warnings {
        warn!(warning = %warning, "configuration warning");
    }

    // The base observer is what hooks themselves report through, so a failing
    // hook cannot produce events that re-enter it.
    let base: Arc<dyn EventObserver> = Arc::new(TracingEvents);
    let (observer, dispatchers, exported) = match resolved.hooks {
        Some(ResolvedHooks { config, client }) => {
            let (hooks, dispatchers) =
                hooks::build(config, client, Events::new(Arc::clone(&base)))?;
            (
                Arc::new(HookObserver::new(hooks.clone(), base)) as Arc<dyn EventObserver>,
                Some(dispatchers),
                Some(hooks),
            )
        }
        None => (base, None, None),
    };

    let mut node = Node::new(
        resolved.node,
        resolved.authenticator,
        Events::new(observer),
        resolved.playback,
    )?;
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
