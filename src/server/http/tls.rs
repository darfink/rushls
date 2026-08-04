//! TLS termination with certificates that rotate underneath a running origin.
//!
//! Two things make this more than a wrapper around [`tokio_rustls`].
//!
//! The first is reload. A `ServerConfig` is built once and never rebuilt;
//! every handshake asks [`CertificateResolver`] which key to present, and that
//! resolver is one [`ArcSwap`] deep. Rotating a certificate is therefore a
//! single atomic store: live connections keep the key they negotiated with,
//! the next handshake picks up the new one, and nothing is dropped or
//! rebound. Only the certificate and key can change this way — the ALPN list
//! and the crypto provider are fixed at bind time, because nothing rotates
//! them on a schedule.
//!
//! The second is that the handshake must not happen on the accept path.
//! [`axum::serve::Listener::accept`] is a single future the server awaits in
//! sequence, so terminating TLS inside it would let one slow client hold up
//! every other connection. Handshakes run as spawned tasks under a deadline
//! and a fixed ceiling instead.

use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::observe::{Events, NodeEvent, ProcessMeters, Protocol};

/// Advertised so a browser can reach HTTP/2, which is what Apple's Low-Latency
/// profile expects. Order is preference order: h2 first, HTTP/1.1 as the
/// fallback for clients that cannot.
const ALPN_PROTOCOLS: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// How long the watcher coalesces filesystem activity before reloading.
///
/// Rotation is never one atomic act — certbot writes two files, a Kubernetes
/// secret mount swaps a symlinked directory. Reacting to the first event would
/// reliably load a new certificate against the old key.
const DEBOUNCE: Duration = Duration::from_secs(1);

/// Where the certificate and its key live, and how patient the listener is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsSettings {
    /// PEM, leaf first, followed by any intermediates.
    pub certificate: PathBuf,
    /// PEM, PKCS#8, PKCS#1, or SEC1.
    pub key: PathBuf,
    /// Bounds a connection that completes TCP and then stalls mid-handshake.
    pub handshake_timeout: Duration,
    /// Caps handshakes in flight at once, which is what stops a flood from
    /// growing the task set without bound.
    pub maximum_pending_handshakes: usize,
}

impl Default for TlsSettings {
    fn default() -> Self {
        Self {
            certificate: PathBuf::new(),
            key: PathBuf::new(),
            handshake_timeout: Duration::from_secs(10),
            maximum_pending_handshakes: 256,
        }
    }
}

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("could not read {path}: {source}")]
    Unreadable {
        path: PathBuf,
        source: rustls::pki_types::pem::Error,
    },
    #[error("{path} contains no certificate")]
    Empty { path: PathBuf },
    /// Covers a key that will not parse, a key belonging to some other
    /// certificate, and a certificate rustls will not serve at all — an X.509
    /// v1 certificate being the one an operator is most likely to produce by
    /// accident, since `openssl req -x509` emits one unless asked otherwise.
    #[error("the certificate and key are not usable: {0}")]
    Unusable(rustls::Error),
    #[error("TLS could not be configured: {0}")]
    Configuration(rustls::Error),
    #[error("could not watch {path} for certificate changes: {source}")]
    Unwatchable {
        path: PathBuf,
        source: notify::Error,
    },
}

impl From<TlsError> for io::Error {
    fn from(error: TlsError) -> Self {
        Self::other(error.to_string())
    }
}

/// Answers every handshake from a slot that can be replaced underneath it.
#[derive(Debug)]
struct CertificateResolver(ArcSwap<CertifiedKey>);

impl ResolvesServerCert for CertificateResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.load_full())
    }
}

/// Reads a certificate and key, and proves they belong together.
///
/// [`CertifiedKey::from_der`] parses the key with the provider and compares
/// its `SubjectPublicKeyInfo` against the leaf certificate's, so a rotation
/// caught half-written is rejected here rather than at the next handshake.
fn load(settings: &TlsSettings, provider: &CryptoProvider) -> Result<CertifiedKey, TlsError> {
    let chain = CertificateDer::pem_file_iter(&settings.certificate)
        .and_then(std::iter::Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|source| TlsError::Unreadable {
            path: settings.certificate.clone(),
            source,
        })?;
    if chain.is_empty() {
        return Err(TlsError::Empty {
            path: settings.certificate.clone(),
        });
    }
    let key =
        PrivateKeyDer::from_pem_file(&settings.key).map_err(|source| TlsError::Unreadable {
            path: settings.key.clone(),
            source,
        })?;
    CertifiedKey::from_der(chain, key, provider).map_err(TlsError::Unusable)
}

/// A bound TLS listener plus the watch that keeps its certificate current.
///
/// The watcher is owned here because dropping it silently stops reloads; tying
/// its lifetime to the listener's means the two cannot get separated.
pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
    settings: TlsSettings,
    meters: ProcessMeters,
    events: Events,
    _watcher: CertificateWatch,
}

impl TlsListener {
    /// Terminates TLS on an already-bound listener, loading the initial
    /// certificate and starting the watch for rotations.
    ///
    /// Failing to load at startup is fatal, unlike failing to reload later: a
    /// process that comes up without a usable certificate has nothing to serve.
    pub fn new(
        tcp: TcpListener,
        settings: TlsSettings,
        meters: ProcessMeters,
        events: Events,
    ) -> Result<Self, TlsError> {
        // Carried explicitly rather than through `CryptoProvider::install_default`
        // so nothing here depends on a process-global having been set first.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let resolver = Arc::new(CertificateResolver(ArcSwap::from_pointee(load(
            &settings, &provider,
        )?)));
        events.emit(NodeEvent::CertificateLoaded {
            certificate: settings.certificate.clone(),
        });

        let mut config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Configuration)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
        config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|name| name.to_vec()).collect();

        let watcher =
            CertificateWatch::start(settings.clone(), resolver, provider, events.clone())?;

        Ok(Self {
            tcp,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshakes: JoinSet::new(),
            settings,
            meters,
            events,
            _watcher: watcher,
        })
    }

    /// Mirrors the trait method so callers need not import the trait to ask.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                // Accepting stops at capacity rather than queueing: refusing to
                // take the next connection is backpressure the kernel already
                // knows how to apply, and it needs no semaphore to express.
                accepted = self.tcp.accept(),
                    if self.handshakes.len() < self.settings.maximum_pending_handshakes =>
                {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        // Mirrors what axum's own `TcpListener` impl does: an
                        // accept error is never fatal to the listener, and
                        // spinning on it would burn a core.
                        Err(error) => {
                            self.events.emit(NodeEvent::ListenerAcceptFailed {
                                protocol: Protocol::Https,
                                reason: error.to_string(),
                            });
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let acceptor = self.acceptor.clone();
                    let deadline = self.settings.handshake_timeout;
                    let meters = self.meters.clone();
                    self.handshakes.spawn(async move {
                        if let Ok(Ok(stream)) = timeout(deadline, acceptor.accept(stream)).await {
                            meters.tls_handshake_completed();
                            Some((stream, peer))
                        } else {
                            // Neither outcome is reported as an event: the rate
                            // is whatever a remote peer chooses to make it.
                            meters.tls_handshake_failed();
                            None
                        }
                    });
                }

                Some(joined) = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                    if let Ok(Some(ready)) = joined {
                        return ready;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Keeps the debouncer and its pump alive for as long as the listener lives.
struct CertificateWatch {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for CertificateWatch {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl CertificateWatch {
    fn start(
        settings: TlsSettings,
        resolver: Arc<CertificateResolver>,
        provider: Arc<CryptoProvider>,
        events: Events,
    ) -> Result<Self, TlsError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
            // The receiver going away means the listener is gone; there is
            // nothing left to reload for.
            let _ = sender.send(result);
        })
        .map_err(|source| TlsError::Unwatchable {
            path: settings.certificate.clone(),
            source,
        })?;

        // Watching the containing directories rather than the files. Rotation
        // replaces rather than rewrites — certbot renames into place, a
        // Kubernetes secret mount swaps a `..data` symlink — and a watch on
        // the file itself is left pointing at an inode nobody will touch
        // again.
        for directory in watched_directories(&settings) {
            debouncer
                .watch(&directory, RecursiveMode::NonRecursive)
                .map_err(|source| TlsError::Unwatchable {
                    path: directory.clone(),
                    source,
                })?;
        }

        let pump = tokio::spawn(reload_on_change(
            settings, resolver, provider, events, receiver,
        ));
        Ok(Self {
            _debouncer: debouncer,
            pump,
        })
    }
}

/// The directories holding the pair, deduplicated when both files share one.
fn watched_directories(settings: &TlsSettings) -> Vec<PathBuf> {
    let mut directories = Vec::with_capacity(2);
    for path in [&settings.certificate, &settings.key] {
        let directory = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
}

async fn reload_on_change(
    settings: TlsSettings,
    resolver: Arc<CertificateResolver>,
    provider: Arc<CryptoProvider>,
    events: Events,
    mut changes: mpsc::UnboundedReceiver<DebounceEventResult>,
) {
    while let Some(result) = changes.recv().await {
        // Any activity in the directory is a reason to re-read, without
        // inspecting which paths the events name.
        //
        // Filtering on the configured paths is the obvious design and it is
        // wrong. A watcher reports the path the platform resolved, and a
        // Kubernetes secret mount never resolves to the name we were given:
        // `tls.crt` is a symlink into `..data`, itself a symlink to a
        // timestamped directory, and rotation swaps the *symlink* — so every
        // event names `..data` or a directory we have never heard of, and a
        // path filter discards all of them. The certificate would then never
        // reload, in exactly the deployment that needs it most, and the
        // symptom would be nothing happening at all.
        //
        // Re-reading unconditionally costs two file reads on a dedicated
        // secret mount, already coalesced by the debounce, and the comparison
        // below keeps a spurious wake from being mistaken for a rotation.
        if let Err(errors) = result {
            let reason = errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            events.emit(NodeEvent::CertificateWatchLost { reason });
            continue;
        }

        // Blocking reads, moved off the runtime because a rotation on a slow
        // or network-backed volume would otherwise stall a worker thread.
        let settings_for_load = settings.clone();
        let provider_for_load = Arc::clone(&provider);
        let loaded =
            tokio::task::spawn_blocking(move || load(&settings_for_load, &provider_for_load)).await;

        match loaded {
            Ok(Ok(certified)) => {
                // Unchanged material is the common case now that any activity
                // wakes this loop. Swapping anyway would be harmless, but
                // reporting it would turn the event stream — the thing an
                // operator reads to answer "did the rotation take?" — into
                // noise.
                if certified.cert == resolver.0.load().cert {
                    continue;
                }
                resolver.0.store(Arc::new(certified));
                events.emit(NodeEvent::CertificateLoaded {
                    certificate: settings.certificate.clone(),
                });
            }
            // The previously loaded pair stays in place. A rotation that
            // produced an unusable file is a reason to keep serving, not to
            // start failing every handshake.
            Ok(Err(error)) => events.emit(NodeEvent::CertificateRejected {
                certificate: settings.certificate.clone(),
                reason: error.to_string(),
            }),
            Err(error) => events.emit(NodeEvent::CertificateWatchLost {
                reason: error.to_string(),
            }),
        }
    }

    // The channel closes when the debouncer is dropped, which happens with the
    // listener. Reaching here during a shutdown is ordinary; reaching it while
    // still serving means rotations are no longer observed.
    events.emit(NodeEvent::CertificateWatchLost {
        reason: "the certificate watch ended".into(),
    });
}

#[cfg(test)]
mod tests {
    use super::{super::fixtures::write_pair, *};

    fn provider() -> CryptoProvider {
        rustls::crypto::ring::default_provider()
    }

    #[test]
    fn a_matching_pair_loads() {
        let directory = crate::server::http::fixtures::scratch("matching");
        let (settings, _) = write_pair(&directory, "origin.test");

        load(&settings, &provider()).expect("a self-signed pair is usable");
    }

    #[test]
    fn a_key_from_a_different_certificate_is_refused() {
        let directory = crate::server::http::fixtures::scratch("mismatched");
        let (settings, _) = write_pair(&directory, "origin.test");
        let other = crate::server::http::fixtures::scratch("mismatched-other");
        let (foreign, _) = write_pair(&other, "other.test");
        std::fs::copy(&foreign.key, &settings.key).expect("the foreign key replaces the real one");

        assert!(matches!(
            load(&settings, &provider()),
            Err(TlsError::Unusable(_))
        ));
    }

    #[test]
    fn a_truncated_certificate_is_refused_rather_than_loaded_empty() {
        let directory = crate::server::http::fixtures::scratch("truncated");
        let (settings, _) = write_pair(&directory, "origin.test");
        std::fs::write(&settings.certificate, b"").expect("the certificate is truncated");

        assert!(matches!(
            load(&settings, &provider()),
            Err(TlsError::Empty { .. })
        ));
    }

    #[test]
    fn a_pair_sharing_one_directory_is_watched_once() {
        let settings = TlsSettings {
            certificate: PathBuf::from("/etc/tls/fullchain.pem"),
            key: PathBuf::from("/etc/tls/privkey.pem"),
            ..TlsSettings::default()
        };

        assert_eq!(watched_directories(&settings), [PathBuf::from("/etc/tls")]);
    }
}
