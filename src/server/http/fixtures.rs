//! Test material for the HTTP surface.

use std::{
    fmt::Write as _,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::delivery::{
    Origin,
    hls::{
        StreamLease, StreamStore,
        fixtures::{initialization, lease, video, write, write_segment},
        service::{Config as HlsConfig, Service as HlsService},
    },
};
use crate::domain::SessionId;
use crate::observe::{
    EventObserver, Events, HlsMeters, NodeEvent, OriginMeters, ProcessMeters, SessionEvent,
};
use crate::server::{
    metrics::{MetricsEndpoint, MetricsReader, MetricsToken},
    runtime::ViewerApplication,
};
use crate::session::Registry;

use super::{
    HttpConfig, HttpListener, PlaybackGate, Readiness, TcpHttpListener, TlsSettings, serve,
};

/// The production HLS/media composition used by HTTP integration tests.
pub(crate) fn application(store: &StreamStore) -> Arc<ViewerApplication> {
    application_with(store, HlsConfig::default())
}

pub(crate) fn application_with(store: &StreamStore, hls: HlsConfig) -> Arc<ViewerApplication> {
    let origin = Arc::new(Origin::new(store.clone()));
    let hls = Arc::new(HlsService::new(Arc::clone(&origin), hls));
    Arc::new(ViewerApplication::new(origin, hls))
}

pub fn metrics_reader(store: &StreamStore) -> MetricsReader {
    MetricsReader::new(
        ProcessMeters::default(),
        OriginMeters::default(),
        HlsMeters::default(),
        Registry::default(),
        store.clone(),
    )
}

pub enum MetricsMode {
    Disabled,
    Enabled(Option<MetricsToken>),
}

/// Listener and service settings for tests that serve a real HTTP connection.
pub struct HttpOptions {
    pub config: HttpConfig,
    pub hls: HlsConfig,
    pub metrics: MetricsMode,
    pub playback: Option<Arc<PlaybackGate>>,
    pub readiness: Readiness,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            config: HttpConfig::default(),
            hls: HlsConfig::default(),
            metrics: MetricsMode::Disabled,
            playback: None,
            readiness: Readiness::ready(),
        }
    }
}

impl HttpOptions {
    pub async fn start(mut self) -> HttpHarness {
        let store = StreamStore::default();
        self.hls.query_variables |= self.playback.is_some();
        let application = application_with(&store, std::mem::take(&mut self.hls));
        self.start_application(application, store).await
    }

    /// Uses an application that already has a publication, such as a node
    /// whose ingress pipeline the test exercised before starting HTTP.
    pub async fn start_application(
        self,
        application: Arc<ViewerApplication>,
        store: StreamStore,
    ) -> HttpHarness {
        let tcp = ephemeral_tcp().await;
        let address = tcp.local_addr().expect("the listener is bound");
        self.spawn(TcpHttpListener::from(tcp), address, application, store)
    }

    pub fn spawn(
        self,
        listener: impl HttpListener,
        address: SocketAddr,
        application: Arc<ViewerApplication>,
        store: StreamStore,
    ) -> HttpHarness {
        let metrics = match self.metrics {
            MetricsMode::Enabled(token) => {
                let mut reader = metrics_reader(&store);
                if let Some(gate) = &self.playback {
                    reader = reader.with_playback(gate.meters());
                }
                Some(MetricsEndpoint::new(reader, token))
            }
            MetricsMode::Disabled => None,
        };
        let (shutdown, signal) = oneshot::channel();
        let served = tokio::spawn(serve(
            listener,
            application,
            self.config,
            metrics,
            self.playback,
            self.readiness,
            async {
                let _ = signal.await;
            },
        ));
        HttpHarness {
            address,
            store,
            shutdown: Some(shutdown),
            served,
        }
    }
}

pub async fn ephemeral_tcp() -> TcpListener {
    TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port is available")
}

/// A running HTTP origin, its store, and the handle that stops it.
pub struct HttpHarness {
    pub address: SocketAddr,
    pub store: StreamStore,
    shutdown: Option<oneshot::Sender<()>>,
    served: JoinHandle<std::io::Result<()>>,
}

impl HttpHarness {
    pub async fn start() -> Self {
        HttpOptions::default().start().await
    }

    pub async fn advance_retention(&self, duration: Duration) {
        // Advance between requests only; auto-advancing a real socket's
        // timeout while the OS delivers its response would mask HTTP behavior.
        tokio::time::pause();
        tokio::time::advance(duration).await;
        tokio::time::resume();
        self.store.maintain();
    }

    pub fn begin_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    pub async fn stop(mut self) {
        self.begin_shutdown();
        self.served
            .await
            .expect("HTTP task did not panic")
            .expect("HTTP server stopped cleanly");
    }
}

/// Publishes enough video to serve playlists and fetch initialization,
/// segments, and parts. The caller can keep the publication open for reloads.
pub fn publish_camera(store: &StreamStore) -> StreamLease {
    let publication = lease(store, vec![video(0)]);
    write(&publication, initialization(0, 1));
    write_segment(&publication, 0, 0, 0);
    publication
}

/// A raw HTTP/1.1 response, parsed just enough to assert on.
pub struct Reply {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn parse_head(head: &[u8]) -> Self {
        let head = String::from_utf8_lossy(head);
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("the status line parses");
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect();
        Self {
            status,
            headers,
            body: Vec::new(),
        }
    }

    /// Every value sent for one field name, joined the way a comma-separated
    /// list field means them.
    ///
    /// `Vary` arrives as more than one line here: the origin sets its own
    /// content-negotiation field and the CORS middleware adds its own, which
    /// HTTP treats as equivalent to one combined list.
    pub fn joined(&self, name: &str) -> Option<String> {
        let values: Vec<&str> = self
            .headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Speaks HTTP/1.1 by hand rather than pulling in a client crate.
///
/// Each request is a few lines, and a hand-written one can assert on exactly
/// what came back — including that a HEAD carried no body at all, which a
/// client library would hide behind its own response type.
pub async fn request(
    address: SocketAddr,
    method: &str,
    target: &str,
    extra: &[(&str, &str)],
) -> Reply {
    let stream = TcpStream::connect(address).await.expect("the origin is up");
    request_on(stream, method, target, extra).await
}

pub async fn request_on(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    method: &str,
    target: &str,
    extra: &[(&str, &str)],
) -> Reply {
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: origin\r\nConnection: close\r\n");
    for (name, value) in extra {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .expect("the request is sent");

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("the response completes");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has headers");
    let mut reply = Reply::parse_head(&raw[..split]);
    reply.body = raw.split_off(split + 4);
    reply
}

pub const TLS_HOST: &str = "origin.test";

pub fn self_signed(name: &str) -> rcgen::CertifiedKey<rcgen::KeyPair> {
    rcgen::generate_simple_self_signed([name.to_owned()])
        .expect("a self-signed certificate is generated")
}

/// Lets the client inspect any rotated leaf while still verifying the
/// handshake signature over that leaf with the configured crypto provider.
#[derive(Debug)]
struct TestCertificateVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for TestCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub async fn tls_connect(
    address: SocketAddr,
    offer: &[&[u8]],
) -> Result<TlsStream<TcpStream>, std::io::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(TestCertificateVerifier(Arc::clone(&provider)));
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the default versions are supported")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = offer.iter().map(|name| name.to_vec()).collect();

    let stream = TcpStream::connect(address).await?;
    let name = ServerName::try_from(TLS_HOST).expect("a valid test name");
    TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
}

/// A self-signed pair written into `directory`, plus the certificate's DER.
///
/// The DER is what a reload test compares: it is the exact bytes a client sees
/// as the peer certificate, so "did the rotation take effect" needs no parsing
/// to answer.
pub fn write_pair(directory: &Path, name: &str) -> (TlsSettings, Vec<u8>) {
    let certified = self_signed(name);
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
/// Writing in place would let a watcher observe a half-written file, which is
/// the very race the debounce exists to absorb — tests should exercise the
/// same shape production sees, not an easier one.
pub fn write_atomically(path: &Path, contents: &[u8]) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, contents).expect("the staged file is written");
    std::fs::rename(&staged, path).expect("the staged file is renamed into place");
}

/// Writes a pair the way a Kubernetes secret mount presents one, and returns
/// the settings naming the stable symlinks plus the new certificate's DER.
///
/// Reproduces the layout exactly, because its shape is the whole point:
///
/// ```text
/// tls.crt -> ..data/tls.crt
/// ..data  -> ..2026_07_29_10_00_00.000
/// ..2026_07_29_10_00_00.000/tls.crt
/// ```
///
/// Rotation writes a fresh timestamped directory and atomically swaps
/// `..data`, so no filesystem event ever names `tls.crt`. Calling this twice
/// on the same directory rotates it the way kubelet does.
#[cfg(unix)]
pub fn write_projected_pair(directory: &Path, name: &str) -> (TlsSettings, Vec<u8>) {
    let certified = self_signed(name);

    let generation = directory.join(format!(
        "..{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&generation).expect("the generation directory is created");
    std::fs::write(generation.join("tls.crt"), certified.cert.pem())
        .expect("the certificate is written");
    std::fs::write(
        generation.join("tls.key"),
        certified.signing_key.serialize_pem(),
    )
    .expect("the key is written");

    // The swap kubelet performs: a symlink is created under a temporary name
    // and renamed over the live one, so readers never see a missing `..data`.
    let data = directory.join("..data");
    let staged = directory.join("..data_tmp");
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(
        generation.file_name().expect("the generation has a name"),
        &staged,
    )
    .expect("the staged link is created");
    std::fs::rename(&staged, &data).expect("the staged link is renamed into place");

    let settings = TlsSettings {
        certificate: directory.join("tls.crt"),
        key: directory.join("tls.key"),
        ..TlsSettings::default()
    };
    for (link, target) in [
        (&settings.certificate, "..data/tls.crt"),
        (&settings.key, "..data/tls.key"),
    ] {
        if std::fs::symlink_metadata(link).is_err() {
            std::os::unix::fs::symlink(target, link).expect("the stable link is created");
        }
    }
    (settings, certified.cert.der().to_vec())
}

/// A private directory for one test, removed if a previous run left it behind.
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("rushls-tls-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("the scratch directory is created");
    directory
}

/// Collects process events so a test can assert on what was reported.
#[derive(Default)]
pub struct NodeEventRecorder {
    events: Mutex<Vec<NodeEvent>>,
}

impl NodeEventRecorder {
    /// The recorder and the [`Events`] handle that feeds it.
    pub fn install() -> (Arc<Self>, Events) {
        let recorder = Arc::new(Self::default());
        let events = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>);
        (recorder, events)
    }

    pub fn recorded(&self) -> Vec<NodeEvent> {
        self.events.lock().clone()
    }

    pub async fn listener_address(
        &self,
        protocol: crate::observe::Protocol,
    ) -> Result<std::net::SocketAddr, tokio::time::error::Elapsed> {
        self.wait_for(Duration::from_secs(5), |event| match event {
            NodeEvent::ListenerBound {
                protocol: bound,
                address,
            } if *bound == protocol => Some(*address),
            _ => None,
        })
        .await
    }

    pub async fn certificate_rejected(
        &self,
        timeout: Duration,
    ) -> Result<(), tokio::time::error::Elapsed> {
        self.wait_for(timeout, |event| {
            matches!(event, NodeEvent::CertificateRejected { .. }).then_some(())
        })
        .await
    }

    /// Waits for evidence emitted by the normal listener or reload path.
    async fn wait_for<T>(
        &self,
        timeout: Duration,
        pick: impl Fn(&NodeEvent) -> Option<T>,
    ) -> Result<T, tokio::time::error::Elapsed> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(found) = self.events.lock().iter().find_map(&pick) {
                    return found;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
    }
}

impl EventObserver for NodeEventRecorder {
    fn observe(&self, _session: SessionId, _event: SessionEvent) {}

    fn observe_node(&self, event: NodeEvent) {
        self.events.lock().push(event);
    }
}

/// Real CMAF bytes with one AAC priming frame and video starting 500 ms later.
/// The shared presentation origin must survive both muxing and HTTP projection.
pub fn offset_cmaf() -> Result<
    (
        Arc<crate::mux::PackagedPresentation>,
        Vec<crate::mux::PackagedMedia>,
    ),
    Box<dyn std::error::Error>,
> {
    use crate::{
        admission::StreamPolicy,
        domain::{
            AudioTiming, AudioTrim, Codec, FrameRate, MediaKind, MediaParameters, Payload,
            Timebase, TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{AudioSample, NormalizedMedia, VideoSample, validate},
        mux::{
            FinishReason, MuxerStartRequest,
            fixtures::{
                AAC_EXTRADATA, AAC_FRAME, H264_EXTRADATA, H264_IDR, H264_P, discarded_events,
            },
        },
        segment::{SegmentationPlan, fixtures::PlanBuilder},
    };
    let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
    let video_base = Timebase::new(nz::u32!(1), nz::u32!(16_384));
    let audio = TrackBuilder::new(0, MediaKind::Audio)
        .timebase(audio_base)
        .parameters(MediaParameters::Audio {
            sample_rate: nz::u32!(48_000),
            channels: nz::u16!(1),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: None,
            timing: AudioTiming {
                initial_padding_samples: 1_024,
                ..AudioTiming::default()
            },
        })
        .codec(Codec::Aac)
        .codec_extradata(AAC_EXTRADATA.to_vec())
        .build();
    let video = TrackBuilder::new(1, MediaKind::Video)
        .timebase(video_base)
        .parameters(MediaParameters::Video {
            width: nz::u32!(16),
            height: nz::u32!(16),
            frame_rate: Some(FrameRate::new(nz::u32!(2), nz::u32!(1))),
            video_delay: 0,
        })
        .codec_extradata(H264_EXTRADATA.to_vec())
        .build();
    let input = validate(&catalog(vec![audio, video]), &StreamPolicy::permissive())?;
    let segmentation = SegmentationPlan::new(
        &input,
        vec![
            PlanBuilder::new(0, audio_base, nz::u64!(49_152))
                .part(nz::u32!(2), nz::u64!(2_048))
                .build(),
            PlanBuilder::new(1, video_base, nz::u64!(16_384))
                .part(nz::u32!(1), nz::u64!(8_192))
                .presentation_origin(-8_192)
                .build(),
        ],
    )?;
    let mut started = crate::mux::start_muxer(MuxerStartRequest {
        presentation: &input,
        segmentation: &segmentation,
        time_anchor: crate::delivery::hls::fixtures::anchor(),
        events: &discarded_events(),
        budget: &crate::domain::PipelineBudget::unlimited(),
    })?;
    let mut media = Vec::new();
    for frame in -1..4 {
        started.muxer.push(
            NormalizedMedia::Audio(AudioSample {
                track_id: TrackId(0),
                codec: Codec::Aac,
                pts: frame * 1_024,
                duration: 1_024,
                trim: AudioTrim {
                    leading_samples: if frame == -1 { 1_024 } else { 0 },
                    trailing_samples: 0,
                },
                payload: Payload::from(AAC_FRAME.to_vec()),
            }),
            &mut media,
        )?;
    }
    for frame in 0..2 {
        started.muxer.push(
            NormalizedMedia::Video(VideoSample {
                track_id: TrackId(1),
                codec: Codec::H264,
                pts: frame * 8_192,
                dts: frame * 8_192,
                duration: 8_192,
                random_access: frame == 0,
                payload: Payload::from(if frame == 0 {
                    H264_IDR.to_vec()
                } else {
                    H264_P.to_vec()
                }),
            }),
            &mut media,
        )?;
    }
    started.muxer.finish(FinishReason::Final, &mut media)?;
    Ok((started.presentation, media))
}

/// Completes a real QUIC handshake against a pinned test leaf.
async fn quic_leaf(
    address: std::net::SocketAddr,
    trusted: &[u8],
) -> Result<rustls::pki_types::CertificateDer<'static>, Box<dyn std::error::Error>> {
    use web_transport_quinn::quinn::{self, crypto::rustls::QuicClientConfig};

    let mut roots = rustls::RootCertStore::empty();
    roots.add(trusted.to_vec().into())?;
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls)?,
    )));
    let connection = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        endpoint.connect(address, TLS_HOST)?,
    )
    .await??;
    let identity = connection
        .peer_identity()
        .expect("QUIC used certificate authentication");
    let chain = identity
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .expect("rustls peer identity");
    let leaf = chain.first().expect("a peer leaf").clone();
    endpoint.close(0_u32.into(), b"test complete");
    Ok(leaf)
}

/// A production node with live QUIC and cleartext HTTP listeners.
pub struct QuicHarness {
    pub address: SocketAddr,
    quic: SocketAddr,
    pub events: Arc<NodeEventRecorder>,
    shutdown: oneshot::Sender<()>,
    serving: JoinHandle<Result<(), crate::server::RuntimeError>>,
}

impl QuicHarness {
    pub async fn start(settings: TlsSettings) -> Result<Self, Box<dyn std::error::Error>> {
        use crate::{
            admission::{OpenStreamAuthenticator, StreamPolicy},
            observe::Protocol,
            server::{Node, NodeConfig},
        };

        let (recorder, events) = NodeEventRecorder::install();
        let bind = "127.0.0.1:0".parse()?;
        let mut config = NodeConfig {
            rtmp_address: bind,
            srt_address: bind,
            moq_address: Some(bind),
            http_address: Some(bind),
            ..NodeConfig::default()
        };
        config.moq.tls = Some(settings);
        let node = Node::new(
            config,
            Arc::new(OpenStreamAuthenticator::new(StreamPolicy::permissive())),
            events,
            None,
        )?;
        let (shutdown, signal) = oneshot::channel();
        let serving = tokio::spawn(node.serve(async {
            let _ = signal.await;
        }));
        let address = recorder.listener_address(Protocol::Http).await?;
        let quic = recorder.listener_address(Protocol::Moq).await?;
        Ok(Self {
            address,
            quic,
            events: recorder,
            shutdown,
            serving,
        })
    }

    pub async fn stop(self) -> Result<(), Box<dyn std::error::Error>> {
        self.shutdown.send(()).expect("node is running");
        self.serving.await??;
        Ok(())
    }

    pub async fn leaf_hash(&self, expected: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
        let leaf = quic_leaf(self.quic, expected).await?;
        assert_eq!(leaf.as_ref(), expected);
        Ok(super::leaf_fingerprint(&leaf))
    }

    pub async fn fingerprint(&self, headers: &[(&str, &str)]) -> Reply {
        let reply = request(self.address, "GET", "/certificate.sha256", headers).await;
        assert_eq!(reply.status, 200, "GET /certificate.sha256");
        reply
    }
}
