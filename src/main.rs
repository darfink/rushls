use std::{error::Error, sync::Arc};

use rushls::{
    domain::{SessionId, StreamId},
    hooks::{self, HookObserver},
    observe::{EventObserver, Events, NodeEvent, SessionEvent, StreamEvent},
    server::{AppConfig, Node, ResolvedHooks},
};
use tokio::sync::watch;

struct StderrEvents;

impl EventObserver for StderrEvents {
    fn observe(&self, session: SessionId, event: SessionEvent) {
        eprintln!("session {session:?}: {event:?}");
    }

    fn observe_stream(&self, stream: StreamId, event: StreamEvent) {
        match event {
            StreamEvent::Available => eprintln!("stream {stream:?} is playable"),
            StreamEvent::Retired => eprintln!("stream {stream:?} is no longer reachable"),
        }
    }

    fn observe_node(&self, event: NodeEvent) {
        match event {
            NodeEvent::ShuttingDown => {
                eprintln!("shutting down");
            }
            NodeEvent::ListenerBound { protocol, address } => {
                eprintln!("{protocol} listening on {address}");
            }
            NodeEvent::CertificateLoaded { certificate } => {
                eprintln!("serving the certificate at {}", certificate.display());
            }
            NodeEvent::CertificateRejected {
                certificate,
                reason,
            } => eprintln!(
                "keeping the previous certificate; {} was rejected: {reason}",
                certificate.display()
            ),
            NodeEvent::CertificateWatchLost { reason } => {
                eprintln!("certificate rotations will no longer be noticed: {reason}");
            }
            NodeEvent::HookEventDropped {
                hook,
                event,
                kind,
                reason,
                detail,
            } => eprintln!("hook {hook}: dropped {kind} {event}, {reason} ({detail})"),
            NodeEvent::HookEventsAbandoned { hook, dropped } => {
                eprintln!("hook {hook}: abandoned {dropped} undelivered events at shutdown");
            }
            NodeEvent::HookDeliveryOutcomesUnknown { hook, count } => {
                eprintln!("hook {hook}: {count} deliveries had unknown outcomes at shutdown");
            }
            NodeEvent::HookEventUnrenderable { reason } => {
                eprintln!("a lifecycle event could not be rendered: {reason}");
            }
            NodeEvent::PublisherHandshakeFailed { protocol, reason } => {
                eprintln!("{protocol} handshake rejected: {reason}");
            }
            NodeEvent::PublisherSessionFailed { protocol, reason } => {
                eprintln!("{protocol} publishing session failed: {reason}");
            }
            NodeEvent::ConnectionTaskPanicked { protocol, reason } => {
                eprintln!("{protocol} connection task panicked: {reason}");
            }
            NodeEvent::ListenerAddressUnavailable { protocol, reason } => {
                eprintln!("{protocol} listener bound but has no local address: {reason}");
            }
            NodeEvent::ListenerAcceptFailed { protocol, reason } => {
                eprintln!("{protocol} listener could not accept: {reason}");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let resolved = AppConfig::load()
        .and_then(AppConfig::resolve)
        .unwrap_or_else(|error| error.exit());

    // The base observer is what hooks themselves report through, so a failing
    // hook cannot produce events that re-enter it.
    let base: Arc<dyn EventObserver> = Arc::new(StderrEvents);
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
