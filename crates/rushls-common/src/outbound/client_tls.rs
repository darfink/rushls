//! Mutual TLS material for one outbound destination.
//!
//! Admission and hooks both call operator-run services over a network the
//! operator may not consider private, and the trust question is the same on
//! each: prove this node to the service with a client certificate, and prove
//! the service to this node by pinning its issuer. Both applications configure
//! it with the same three keys, `client_cert`, `client_key`, and `ca`; this is
//! what those keys resolve to.

use std::{path::PathBuf, sync::Arc, time::Duration};

use crate::tls::{ClientIdentity, TlsError, TlsObserver, load_roots};
use rustls::pki_types::CertificateDer;
use thiserror::Error;

use crate::outbound::{ClientConfig, HttpClient, OutboundError};

/// The paths one destination configures; all optional.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientTlsFiles {
    /// PEM certificate chain this node presents, leaf first.
    pub certificate: Option<PathBuf>,
    /// PEM private key for that chain.
    pub key: Option<PathBuf>,
    /// PEM authority to trust instead of the platform store.
    pub ca: Option<PathBuf>,
}

/// Why a destination's TLS material is unusable.
#[derive(Debug, Error)]
pub enum ClientTlsError {
    /// Half a pair is a misconfiguration rather than a partial identity:
    /// presenting a certificate needs its key, and a key alone proves nothing.
    /// Refused rather than ignored, because a node that silently presented no
    /// identity would be rejected by the service later, with nothing to
    /// explain why.
    #[error("client_cert is set without client_key")]
    MissingKey,
    #[error("client_key is set without client_cert")]
    MissingCertificate,
    #[error(transparent)]
    Tls(#[from] TlsError),
}

impl ClientTlsFiles {
    /// Whether anything here needs a client of its own.
    pub fn is_configured(&self) -> bool {
        self.certificate.is_some() || self.key.is_some() || self.ca.is_some()
    }

    /// Loads the identity and roots, starting a watch for identity rotations.
    ///
    /// The watch lives in the returned [`ClientTls`]; hold it for as long as
    /// its clients are used, or rotations stop being noticed. With a client
    /// certificate configured this must run inside a Tokio runtime, which
    /// drives the watch.
    pub fn load<O: TlsObserver>(&self, observer: Arc<O>) -> Result<ClientTls, ClientTlsError> {
        let identity = match (&self.certificate, &self.key) {
            (Some(certificate), Some(key)) => Some(ClientIdentity::new(
                certificate.clone(),
                key.clone(),
                observer,
            )?),
            (None, None) => None,
            (Some(_), None) => return Err(ClientTlsError::MissingKey),
            (None, Some(_)) => return Err(ClientTlsError::MissingCertificate),
        };
        let roots = self.ca.as_deref().map(load_roots).transpose()?;
        Ok(ClientTls { identity, roots })
    }
}

/// Loaded outbound TLS material, holding its own rotation watch.
pub struct ClientTls {
    /// Held for its watch as much as its resolver: dropping it stops reloads.
    identity: Option<ClientIdentity>,
    roots: Option<Vec<CertificateDer<'static>>>,
}

impl ClientTls {
    /// A client presenting this destination's identity, under its own limits.
    ///
    /// Deliberately not drawn from a shared connector: an identity and a
    /// pinned authority belong to the destination that configured them, and
    /// pooling connections across destinations would mean presenting one
    /// service's certificate to another. The cost is one connection pool per
    /// destination that asks for mutual TLS, which is what asking for it means.
    pub fn client(
        &self,
        request_timeout: Duration,
        maximum_response_bytes: usize,
    ) -> Result<HttpClient, OutboundError> {
        HttpClient::with_identity(
            ClientConfig::default(),
            self.identity.as_ref().map(ClientIdentity::resolver),
            self.roots.clone(),
        )
        .map(|client| client.with_limits(request_timeout, maximum_response_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::IgnoreTlsEvents;

    fn files(certificate: Option<&str>, key: Option<&str>, ca: Option<&str>) -> ClientTlsFiles {
        ClientTlsFiles {
            certificate: certificate.map(PathBuf::from),
            key: key.map(PathBuf::from),
            ca: ca.map(PathBuf::from),
        }
    }

    #[test]
    fn half_a_pair_is_refused() {
        let observer = Arc::new(IgnoreTlsEvents);
        assert!(matches!(
            files(Some("cert.pem"), None, None).load(Arc::clone(&observer)),
            Err(ClientTlsError::MissingKey)
        ));
        assert!(matches!(
            files(None, Some("key.pem"), None).load(observer),
            Err(ClientTlsError::MissingCertificate)
        ));
    }

    #[test]
    fn nothing_configured_needs_no_client_of_its_own() -> Result<(), ClientTlsError> {
        assert!(!ClientTlsFiles::default().is_configured());
        assert!(files(None, None, Some("ca.pem")).is_configured());
        // Unconfigured material loads to nothing rather than failing.
        ClientTlsFiles::default().load(Arc::new(IgnoreTlsEvents))?;
        Ok(())
    }

    #[test]
    fn an_unreadable_authority_is_refused() {
        let missing = files(None, None, Some("/nonexistent/rushls-ca.pem"));
        assert!(matches!(
            missing.load(Arc::new(IgnoreTlsEvents)),
            Err(ClientTlsError::Tls(_))
        ));
    }
}
