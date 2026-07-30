use std::{error::Error, sync::Arc};

use rushls::{
    domain::SessionId,
    observe::{EventObserver, Events, NodeEvent, SessionEvent},
    server::{AppConfig, Node},
};

struct StderrEvents;

impl EventObserver for StderrEvents {
    fn observe(&self, session: SessionId, event: SessionEvent) {
        eprintln!("session {session:?}: {event:?}");
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
            NodeEvent::HookEventUnrenderable { reason } => {
                eprintln!("a lifecycle event could not be rendered: {reason}");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let resolved = AppConfig::load()
        .and_then(AppConfig::resolve)
        .unwrap_or_else(|error| error.exit());
    eprintln!("publisher authentication configured");
    let node = Node::new(
        resolved.node,
        resolved.authenticator,
        Events::new(Arc::new(StderrEvents)),
    )?;
    node.serve(shutdown_signal()).await?;
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
