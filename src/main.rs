use std::{error::Error, sync::Arc};

use rushls::{
    admission::{FixedStreamAuthenticator, PublishGrant},
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
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let resolved = AppConfig::load()
        .and_then(AppConfig::resolve)
        .unwrap_or_else(|error| error.exit());
    let stream_id = resolved.stream_id.clone();
    let authenticator = FixedStreamAuthenticator::new(
        resolved.publish_key,
        PublishGrant {
            stream_id: stream_id.clone(),
            principal: resolved.principal,
            policy: resolved.stream_policy,
        },
    );
    eprintln!("publishing to {stream_id}");
    let node = Node::new(
        resolved.node,
        Arc::new(authenticator),
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
