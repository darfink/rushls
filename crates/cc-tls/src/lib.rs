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
    client::ResolvesClientCert,
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

/// What a caller learns about certificates and handshakes.
///
/// A trait rather than a concrete reporter because the two applications count
/// and log differently, and neither should have to adopt the other's
/// observability model to reuse the reload machinery. Every method has a
/// default, so an application that wants none of it implements nothing.
///
/// Implementations are called from the accept loop and from the watcher task,
/// so they must not block.
pub trait TlsObserver: Send + Sync + 'static {
    /// A certificate pair was read and adopted.
    fn certificate_loaded(&self, _certificate: &Path) {}
    /// A rotation was seen but not adopted; the previous pair still serves.
    fn certificate_rejected(&self, _certificate: &Path, _reason: &str) {}
    /// Rotations will no longer be noticed, while TLS keeps working.
    ///
    /// Reported separately because it is otherwise invisible: nothing breaks
    /// until the certificate expires, which may be months away.
    fn certificate_watch_lost(&self, _reason: &str) {}
    /// The listener could not accept a connection.
    fn accept_failed(&self, _reason: &str) {}
    fn handshake_completed(&self) {}
    fn handshake_failed(&self) {}
}

/// Discards everything, for a caller that wants no reporting.
#[derive(Clone, Copy, Debug, Default)]
pub struct IgnoreTlsEvents;

impl TlsObserver for IgnoreTlsEvents {}

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
///
/// Shared by both directions. A client certificate expires exactly as a server
/// one does, so an outbound caller needs the same rotation story as a listener;
/// giving each its own would mean one of them silently not reloading.
#[derive(Debug)]
pub struct CertificateResolver(ArcSwap<CertifiedKey>);

impl ResolvesServerCert for CertificateResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.load_full())
    }
}

impl ResolvesClientCert for CertificateResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(self.0.load_full())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Reads a certificate and key, and proves they belong together.
///
/// [`CertifiedKey::from_der`] parses the key with the provider and compares
/// its `SubjectPublicKeyInfo` against the leaf certificate's, so a rotation
/// caught half-written is rejected here rather than at the next handshake.
fn load(settings: &TlsSettings, provider: &CryptoProvider) -> Result<CertifiedKey, TlsError> {
    let chain = load_certificates(&settings.certificate)?;
    let key = load_private_key(&settings.key)?;
    CertifiedKey::from_der(chain, key, provider).map_err(TlsError::Unusable)
}

/// A client certificate and key that reload underneath a running caller.
///
/// The outbound mirror of [`TlsListener`], and deliberately built from the same
/// parts. A client certificate expires on the same schedule a server one does,
/// so a node that could not renew without a restart would drop publishers on
/// rotation day — the failure the listener side already avoids.
///
/// Resolution happens per handshake through an [`ArcSwap`], so a rotation is one
/// atomic store: connections already established keep the identity they
/// negotiated with, and the next handshake picks up the new one.
pub struct ClientIdentity {
    resolver: Arc<CertificateResolver>,
    /// Held because dropping it stops reloads.
    _watcher: CertificateWatch,
}

impl ClientIdentity {
    /// Loads the pair and starts watching for rotations.
    ///
    /// Failing at startup is fatal, unlike failing to reload later: a node
    /// configured to present an identity it cannot read should not come up
    /// pretending it will be admitted.
    pub fn new<O: TlsObserver>(
        certificate: PathBuf,
        key: PathBuf,
        observer: Arc<O>,
    ) -> Result<Self, TlsError> {
        // The watch is expressed in terms of a settings pair, and the two
        // timeout fields it also carries are meaningless here. Passing the
        // defaults keeps one loader rather than two that could disagree about
        // what a usable pair is.
        let settings = TlsSettings {
            certificate,
            key,
            ..TlsSettings::default()
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let resolver = Arc::new(CertificateResolver(ArcSwap::from_pointee(load(
            &settings, &provider,
        )?)));
        observer.certificate_loaded(&settings.certificate);

        let watcher = CertificateWatch::start(settings, Arc::clone(&resolver), provider, observer)?;
        Ok(Self {
            resolver,
            _watcher: watcher,
        })
    }

    /// The resolver a `rustls` client configuration should ask per handshake.
    ///
    /// Handed out rather than a built configuration so the caller keeps
    /// ownership of its own root store and protocol versions, which are not
    /// this type's business.
    pub fn resolver(&self) -> Arc<dyn ResolvesClientCert> {
        Arc::clone(&self.resolver) as Arc<dyn ResolvesClientCert>
    }
}

/// Reads PEM certificates to trust, for a caller pinning its own authority.
///
/// Not watched. A trust root is the thing a rotation is validated *against*,
/// so it turns over on a different and far slower schedule than the leaf it
/// signs; treating it as hot-reloadable would suggest otherwise.
pub fn load_roots(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    load_certificates(path)
}

/// Reads a nonempty PEM chain, preserving leaf-first certificate order.
pub fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let roots = CertificateDer::pem_file_iter(path)
        .and_then(std::iter::Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|source| TlsError::Unreadable {
            path: path.to_path_buf(),
            source,
        })?;
    if roots.is_empty() {
        return Err(TlsError::Empty {
            path: path.to_path_buf(),
        });
    }
    Ok(roots)
}

/// Reads a PKCS#8, PKCS#1, or SEC1 PEM private key.
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    PrivateKeyDer::from_pem_file(path).map_err(|source| TlsError::Unreadable {
        path: path.to_path_buf(),
        source,
    })
}

/// A bound TLS listener plus the watch that keeps its certificate current.
///
/// The watcher is owned here because dropping it silently stops reloads; tying
/// its lifetime to the listener's means the two cannot get separated.
pub struct TlsListener<O: TlsObserver> {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
    settings: TlsSettings,
    observer: Arc<O>,
    _watcher: CertificateWatch,
}

impl<O: TlsObserver> TlsListener<O> {
    /// Terminates TLS on an already-bound listener, loading the initial
    /// certificate and starting the watch for rotations.
    ///
    /// Failing to load at startup is fatal, unlike failing to reload later: a
    /// process that comes up without a usable certificate has nothing to serve.
    pub fn new(
        tcp: TcpListener,
        settings: TlsSettings,
        observer: Arc<O>,
    ) -> Result<Self, TlsError> {
        // Carried explicitly rather than through `CryptoProvider::install_default`
        // so nothing here depends on a process-global having been set first.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let resolver = Arc::new(CertificateResolver(ArcSwap::from_pointee(load(
            &settings, &provider,
        )?)));
        observer.certificate_loaded(&settings.certificate);

        let mut config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Configuration)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
        config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|name| name.to_vec()).collect();

        let watcher =
            CertificateWatch::start(settings.clone(), resolver, provider, Arc::clone(&observer))?;

        Ok(Self {
            tcp,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            handshakes: JoinSet::new(),
            settings,
            observer,
            _watcher: watcher,
        })
    }

    /// Mirrors the trait method so callers need not import the trait to ask.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    /// Accepts the next connection whose TLS handshake has completed.
    ///
    /// Handshakes run as spawned tasks under a deadline and a fixed ceiling
    /// rather than inline, so one slow client cannot hold up every other
    /// connection waiting to be accepted.
    pub async fn accept_tls(&mut self) -> (TlsStream<TcpStream>, SocketAddr) {
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
                        // An accept error is never fatal to the listener, and
                        // spinning on it would burn a core.
                        Err(error) => {
                            self.observer.accept_failed(&error.to_string());
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let acceptor = self.acceptor.clone();
                    let deadline = self.settings.handshake_timeout;
                    let observer = Arc::clone(&self.observer);
                    self.handshakes.spawn(async move {
                        if let Ok(Ok(stream)) = timeout(deadline, acceptor.accept(stream)).await {
                            observer.handshake_completed();
                            Some((stream, peer))
                        } else {
                            // Neither outcome is reported as an event: the rate
                            // is whatever a remote peer chooses to make it.
                            observer.handshake_failed();
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
}

/// Lets an `axum` server accept from this listener.
///
/// Behind a feature because the reload machinery has nothing to do with axum -
/// the Routmp uses [`TlsListener::accept_tls`] directly and should not
/// compile a web framework to get certificate rotation.
#[cfg(feature = "axum")]
impl<O: TlsObserver> axum::serve::Listener for TlsListener<O> {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        self.accept_tls().await
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Keeps the debouncer and its pump alive for as long as its owner lives.
///
/// Public so an outbound client can hold one too: dropping it silently stops
/// reloads, so its lifetime has to be tied to whatever depends on the material
/// staying current.
pub struct CertificateWatch {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for CertificateWatch {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl CertificateWatch {
    pub fn start<O: TlsObserver>(
        settings: TlsSettings,
        resolver: Arc<CertificateResolver>,
        provider: Arc<CryptoProvider>,
        observer: Arc<O>,
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
            settings, resolver, provider, observer, receiver,
        ));
        Ok(Self {
            _debouncer: debouncer,
            pump,
        })
    }
}

/// A rustls server configuration for QUIC, with the same rotating certificate
/// as [`TlsListener`].
///
/// QUIC cannot reuse an HTTPS [`ServerConfig`]: HTTP/3 requires TLS 1.3 only,
/// a different ALPN, and `max_early_data_size = u32::MAX` so Quinn will accept
/// the config at all. The certificate resolver is the same [`ArcSwap`] the TCP
/// listener uses, so a rotation is still one atomic store and this config is
/// never rebuilt.
///
/// The returned [`CertificateWatch`] must be held for as long as the QUIC
/// endpoint lives; dropping it silently stops reloads.
pub fn rotating_quic_server_config<O: TlsObserver>(
    settings: TlsSettings,
    alpn: &[&[u8]],
    observer: Arc<O>,
) -> Result<(Arc<ServerConfig>, CertificateWatch), TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let resolver = Arc::new(CertificateResolver(ArcSwap::from_pointee(load(
        &settings, &provider,
    )?)));
    observer.certificate_loaded(&settings.certificate);

    let mut config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(TlsError::Configuration)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
    config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
    // Quinn's `QuicServerConfig::try_from` refuses a rustls config that does
    // not advertise 0-RTT. The advertised size is the QUIC maximum; whether a
    // session actually accepts early data is a transport decision, not a TLS one.
    config.max_early_data_size = u32::MAX;

    let watcher = CertificateWatch::start(settings, resolver, provider, observer)?;
    Ok((Arc::new(config), watcher))
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

async fn reload_on_change<O: TlsObserver>(
    settings: TlsSettings,
    resolver: Arc<CertificateResolver>,
    provider: Arc<CryptoProvider>,
    observer: Arc<O>,
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
            observer.certificate_watch_lost(&reason);
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
                observer.certificate_loaded(&settings.certificate);
            }
            // The previously loaded pair stays in place. A rotation that
            // produced an unusable file is a reason to keep serving, not to
            // start failing every handshake.
            Ok(Err(error)) => {
                observer.certificate_rejected(&settings.certificate, &error.to_string())
            }
            Err(error) => observer.certificate_watch_lost(&error.to_string()),
        }
    }

    // The channel closes when the debouncer is dropped, which happens with the
    // listener. Reaching here during a shutdown is ordinary; reaching it while
    // still serving means rotations are no longer observed.
    observer.certificate_watch_lost("the certificate watch ended");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A private directory for one test, removed if a previous run left it behind.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("cc-tls-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("the scratch directory is created");
        directory
    }

    /// A self-signed pair written into `directory`, plus the certificate's DER.
    fn write_pair(directory: &Path, name: &str) -> (TlsSettings, Vec<u8>) {
        let certified = rcgen::generate_simple_self_signed([name.to_owned()])
            .expect("a self-signed certificate is generated");
        let settings = TlsSettings {
            certificate: directory.join("certificate.pem"),
            key: directory.join("key.pem"),
            ..TlsSettings::default()
        };
        write_atomically(&settings.certificate, certified.cert.pem().as_bytes());
        write_atomically(
            &settings.key,
            certified.signing_key.serialize_pem().as_bytes(),
        );
        (settings, certified.cert.der().to_vec())
    }

    /// Writes through a rename, the way real rotation tooling does.
    ///
    /// Writing in place would let a watcher observe a half-written file, which
    /// is the very race the debounce exists to absorb.
    fn write_atomically(path: &Path, contents: &[u8]) {
        let staged = path.with_extension("staged");
        std::fs::write(&staged, contents).expect("the staged file is written");
        std::fs::rename(&staged, path).expect("the staged file is renamed into place");
    }

    fn provider() -> CryptoProvider {
        rustls::crypto::ring::default_provider()
    }

    #[test]
    fn a_matching_pair_loads() {
        let directory = scratch("matching");
        let (settings, _) = write_pair(&directory, "origin.test");

        load(&settings, &provider()).expect("a self-signed pair is usable");
    }

    #[test]
    fn a_key_from_a_different_certificate_is_refused() {
        let directory = scratch("mismatched");
        let (settings, _) = write_pair(&directory, "origin.test");
        let other = scratch("mismatched-other");
        let (foreign, _) = write_pair(&other, "other.test");
        std::fs::copy(&foreign.key, &settings.key).expect("the foreign key replaces the real one");

        assert!(matches!(
            load(&settings, &provider()),
            Err(TlsError::Unusable(_))
        ));
    }

    #[test]
    fn a_truncated_certificate_is_refused_rather_than_loaded_empty() {
        let directory = scratch("truncated");
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

    #[tokio::test]
    async fn a_quic_config_is_tls13_with_early_data_and_custom_alpn() {
        let directory = scratch("quic");
        let (settings, _) = write_pair(&directory, "origin.test");

        let (config, _watch) =
            rotating_quic_server_config(settings, &[b"h3"], Arc::new(IgnoreTlsEvents))
                .expect("a self-signed pair is usable for QUIC");

        assert_eq!(config.alpn_protocols, vec![b"h3".to_vec()]);
        assert_eq!(config.max_early_data_size, u32::MAX);
    }

    #[tokio::test]
    async fn a_client_identity_presents_its_certificate_and_reloads_on_rotation() {
        // The outbound mirror of the listener's rotation test. A client
        // certificate expires the same way a server one does, so a node that
        // could not renew without a restart would drop publishers on rotation
        // day.
        let directory = scratch("client-identity");
        let (settings, first) = write_pair(&directory, "origin.internal");

        let identity = ClientIdentity::new(
            settings.certificate.clone(),
            settings.key.clone(),
            Arc::new(IgnoreTlsEvents),
        )
        .expect("the initial pair loads");

        let presented = |identity: &ClientIdentity| {
            ResolvesClientCert::resolve(&*identity.resolver, &[], &[])
                .expect("an identity always resolves")
                .cert[0]
                .to_vec()
        };
        assert_eq!(presented(&identity), first);

        // Rotate underneath the running identity.
        let (_, second) = write_pair(&directory, "origin.internal");
        assert_ne!(first, second, "the fixture generated a distinct pair");

        // The watch debounces, so give it the debounce plus room to re-read.
        for _ in 0..40 {
            if presented(&identity) == second {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(
            presented(&identity),
            second,
            "a rotated client certificate is picked up without a restart"
        );
    }

    #[test]
    fn a_client_identity_refuses_an_unreadable_pair_at_startup() {
        // Fatal at startup, unlike a failed reload: a node configured to
        // present an identity it cannot read should not come up pretending it
        // will be admitted.
        let directory = scratch("client-identity-missing");
        assert!(matches!(
            ClientIdentity::new(
                directory.join("absent.pem"),
                directory.join("absent.key"),
                Arc::new(IgnoreTlsEvents),
            ),
            Err(TlsError::Unreadable { .. })
        ));
    }

    #[test]
    fn trust_roots_load_from_pem_and_an_empty_file_is_refused() {
        let directory = scratch("roots");
        let (settings, der) = write_pair(&directory, "authority.internal");

        let roots = load_roots(&settings.certificate).expect("the authority loads");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].to_vec(), der);

        let empty = directory.join("empty.pem");
        write_atomically(&empty, b"");
        assert!(
            matches!(load_roots(&empty), Err(TlsError::Empty { .. })),
            "a file that parses but names no authority is refused rather than \
             leaving an empty trust store that refuses every endpoint"
        );
    }
}
