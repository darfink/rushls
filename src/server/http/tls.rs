//! TLS termination with certificates that rotate underneath a running origin.
//!
//! The implementation moved to `cc-tls` so the RTMP proxy shares it. What stays
//! here is the adapter from that crate's [`TlsObserver`] to this node's
//! reporting: the shared crate has no opinion about how an application counts
//! or logs, and neither application should have to adopt the other's.

use std::path::Path;

pub use cc_tls::{TlsError, TlsListener as SharedTlsListener, TlsSettings};

use crate::observe::{Events, NodeEvent, ProcessMeters, Protocol};

/// This node's listener, with its reporting already wired in.
pub type TlsListener = SharedTlsListener<NodeTlsObserver>;

/// Reports certificate and handshake activity the way the rest of this node does.
#[derive(Clone, Debug)]
pub struct NodeTlsObserver {
    meters: ProcessMeters,
    events: Events,
}

impl NodeTlsObserver {
    pub fn new(meters: ProcessMeters, events: Events) -> Self {
        Self { meters, events }
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
            protocol: Protocol::Https,
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
