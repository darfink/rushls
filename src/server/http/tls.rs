//! TLS termination with certificates that rotate underneath a running origin.
//!
//! The implementation moved to `cc-tls` so the Routmp shares it. What stays
//! here is the adapter from that crate's [`TlsObserver`] to this node's
//! reporting: the shared crate has no opinion about how an application counts
//! or logs, and neither application should have to adopt the other's.

use std::{path::Path, sync::Arc};

pub use cc_tls::{TlsError, TlsListener as SharedTlsListener, TlsSettings};

use crate::observe::{Events, NodeEvent, ProcessMeters, Protocol};

/// This node's listener, with its reporting already wired in.
pub type TlsListener = SharedTlsListener<NodeTlsObserver>;

/// Reports certificate and handshake activity the way the rest of this node does.
#[derive(Clone, Debug)]
pub struct NodeTlsObserver {
    meters: ProcessMeters,
    events: Events,
    protocol: Protocol,
}

impl NodeTlsObserver {
    pub fn new(meters: ProcessMeters, events: Events) -> Self {
        Self {
            meters,
            events,
            protocol: Protocol::Https,
        }
    }

    pub fn for_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }
}

impl cc_tls::TlsObserver for NodeTlsObserver {
    fn certificate_loaded(&self, certificate: &Path) {
        self.events.emit(NodeEvent::CertificateLoaded {
            certificate: certificate.to_path_buf(),
        });
    }

    fn certificate_rejected(&self, certificate: &Path, reason: &str) {
        self.events.emit(NodeEvent::CertificateRejected {
            certificate: certificate.to_path_buf(),
            reason: reason.to_owned(),
        });
    }

    fn certificate_watch_lost(&self, reason: &str) {
        self.events.emit(NodeEvent::CertificateWatchLost {
            reason: reason.to_owned(),
        });
    }

    fn accept_failed(&self, reason: &str) {
        self.events.emit(NodeEvent::ListenerAcceptFailed {
            protocol: self.protocol,
            reason: reason.to_owned(),
        });
    }

    fn handshake_completed(&self) {
        self.meters.tls_handshake_completed();
    }

    fn handshake_failed(&self) {
        self.meters.tls_handshake_failed();
    }
}

/// QUIC ingest TLS: TLS 1.3, `h3` ALPN, rotating certificates.
pub(crate) fn rotating_quic_server_config(
    settings: TlsSettings,
    meters: ProcessMeters,
    events: Events,
    protocol: Protocol,
) -> Result<(Arc<rustls::ServerConfig>, cc_tls::CertificateWatch), TlsError> {
    cc_tls::rotating_quic_server_config(
        settings,
        &[web_transport_quinn::ALPN.as_bytes()],
        Arc::new(NodeTlsObserver::new(meters, events).for_protocol(protocol)),
    )
}
