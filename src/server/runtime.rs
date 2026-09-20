use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::watch,
    task::{JoinError, JoinSet},
    time::MissedTickBehavior,
};

use crate::{
    admission::Authenticator,
    delivery::hls::{
        StoreLimits, StorePublisherFactory, StreamStore,
        service::{Config as HlsConfig, Service as HlsService},
    },
    delivery::{
        DeliveryError, DeliveryFailure, Origin, Response as DeliveryResponse, Reuse,
        store::DiskError,
        uri::{MediaResourcePath, parse_media_path},
    },
    hooks::Hooks,
    media::PassThroughNormalizerFactory,
    mux::PassThroughMuxerFactory,
    observe::{Events, NodeEvent, ProcessMeters, Protocol, StreamEvent},
    session::{PendingPublishers, Registry, Services, SessionConfig, StopReason, run_session},
    source::{
        PendingPublish, TransportError,
        transport::{
            moq::{MoqConfig, MoqListener, MoqPendingPublish},
            rtmp::{RtmpConfig, RtmpPendingPublish},
            srt::{SrtConfig, SrtListener, SrtPendingPublish},
        },
    },
};

use super::{
    http::{
        self, Application, HttpConfig, PlaybackGate, PlaybackSettings, PlaybackStartError,
        Readiness, TlsError,
    },
    metrics::{MetricsConfig, MetricsEndpoint, MetricsReader},
};

/// Process-level configuration for one self-contained origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeConfig {
    /// Identifies this node in its own logs and metrics, and as the producer
    /// of hook events.
    ///
    /// Defaults to the hostname, which is right for a single origin and wrong
    /// the moment several sit behind one load balancer and their metric series
    /// become indistinguishable. Several nodes may share one deliberately, in
    /// which case consumers see a single logical producer.
    pub name: Arc<str>,
    /// How long a restart waits for parked viewer requests and queued hook
    /// events before exiting anyway.
    ///
    /// One budget covering both drains, because it answers one operator
    /// question and is sized against an orchestrator's own grace period.
    pub shutdown: Duration,
    pub record: Option<crate::delivery::record::Config>,
    pub rtmp_address: SocketAddr,
    pub srt_address: SocketAddr,
    /// WebTransport ingest. `None` leaves MOQ off, which is the compiled default
    /// so a process can boot without certificates.
    pub moq_address: Option<SocketAddr>,
    /// Cleartext viewer listener. `None` serves HTTPS only.
    pub http_address: Option<SocketAddr>,
    /// TLS viewer listener, bound independently of the cleartext one.
    ///
    /// Separate addresses because enabling TLS should *add* HTTPS rather than
    /// move cleartext off its port, which is what one shared socket did.
    pub https_address: Option<SocketAddr>,
    pub maintenance_interval: Duration,
    pub maximum_sessions: usize,
    pub rtmp: RtmpConfig,
    pub srt: SrtConfig,
    pub moq: MoqConfig,
    pub session: SessionConfig,
    pub store: StoreLimits,
    pub hls: HlsConfig,
    pub http: HttpConfig,
    pub metrics: MetricsConfig,
}

impl NodeConfig {
    /// Connections one ingest listener may be admitting at once.
    ///
    /// Derived from the publisher budget rather than configured. A pending
    /// admission is a precursor to an ingest session, so `publishers` is its
    /// parent: a node holding many *retained* streams has no reason to accept
    /// more unauthenticated sockets, which is why the stream budget is not.
    ///
    /// Counted per listener so a flood on one transport cannot deny admission
    /// on the other, which means the process-wide ceiling is this times the
    /// number of ingest listeners.
    ///
    /// A permit is held across authentication, so this also bounds concurrent
    /// requests to an external admission service.
    pub fn maximum_pending_publishers_per_listener(&self) -> usize {
        self.maximum_sessions.max(1)
    }

    /// Whether `/metrics` is mounted on the cleartext viewer listener.
    fn shares_metrics_with_http(&self) -> bool {
        self.metrics
            .listen
            .is_some_and(|listen| Some(listen) == self.http_address)
    }

    /// Whether `/metrics` is mounted on the HTTPS viewer listener.
    ///
    /// That is how scrapes happen over TLS: there is no separate metrics
    /// certificate. A dedicated metrics port is always cleartext.
    fn shares_metrics_with_https(&self) -> bool {
        self.metrics
            .listen
            .is_some_and(|listen| Some(listen) == self.https_address)
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: node_name(),
            shutdown: Duration::from_secs(10),
            record: None,
            rtmp_address: "0.0.0.0:1935".parse().expect("constant address is valid"),
            srt_address: "0.0.0.0:9000".parse().expect("constant address is valid"),
            moq_address: None,
            http_address: Some("0.0.0.0:8080".parse().expect("constant address is valid")),
            https_address: None,
            maintenance_interval: Duration::from_secs(1),
            maximum_sessions: 256,
            rtmp: RtmpConfig::default(),
            srt: SrtConfig::default(),
            moq: MoqConfig::default(),
            session: SessionConfig::default(),
            store: StoreLimits::default(),
            hls: HlsConfig::default(),
            http: HttpConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid node configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("could not initialize recording: {0}")]
    Recording(std::io::Error),
    #[error("could not bind RTMP at {address}: {source}")]
    BindRtmp {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("could not bind SRT at {address}: {source}")]
    BindSrt {
        address: SocketAddr,
        source: crate::source::TransportError,
    },
    #[error("could not bind MOQ at {address}: {source}")]
    BindMoq {
        address: SocketAddr,
        source: crate::source::TransportError,
    },
    #[error("could not bind HTTP at {address}: {source}")]
    BindHttp {
        address: SocketAddr,
        source: std::io::Error,
    },
    /// Kept apart from [`Self::BindHttp`] because the remedies have nothing in
    /// common: one is an address already in use, the other a certificate an
    /// operator has to go and fix.
    #[error("could not start TLS: {0}")]
    Tls(#[from] TlsError),
    #[error(transparent)]
    Disk(#[from] DiskError),
    #[error("RTMP listener failed: {0}")]
    Rtmp(std::io::Error),
    #[error("SRT listener stopped unexpectedly")]
    SrtStopped,
    #[error("MOQ listener stopped unexpectedly")]
    MoqStopped,
    #[error("HTTP server failed: {0}")]
    Http(std::io::Error),
    #[error("could not start playback authorization: {0}")]
    Playback(#[from] PlaybackStartError),
    #[error("a runtime task failed: {0}")]
    Task(JoinError),
}

/// Fully assembled ingest, packaging, storage, and delivery services.
pub struct Node {
    config: NodeConfig,
    services: Services,
    store: StreamStore,
    origin: Arc<Origin>,
    hls: Arc<HlsService>,
    application: Arc<ViewerApplication>,
    metrics: MetricsReader,
    playback: Option<PlaybackSettings>,
    recorder: Option<crate::delivery::record::Recorder>,
}

/// The protocols sharing one viewer-facing HTTP namespace.
///
/// Runtime is the composition root and therefore the only layer that names
/// both the HTTP port and a manifest adapter. Shared media bypasses adapters;
/// only manifest paths are delegated to HLS.
/// The viewer HTTP application assembled with this node.
///
/// Integration tests bind their own TLS listener and hand this to
/// [`http::serve`](super::http::serve); production wiring does the same inside
/// [`Node::serve`].
pub struct ViewerApplication {
    origin: Arc<Origin>,
    hls: Arc<HlsService>,
}

impl ViewerApplication {
    pub(crate) fn new(origin: Arc<Origin>, hls: Arc<HlsService>) -> Self {
        Self { origin, hls }
    }

    async fn serve_media(
        &self,
        named: MediaResourcePath,
    ) -> Result<DeliveryResponse, DeliveryFailure> {
        let target = self
            .origin
            .stream(&named.stream)
            .and_then(|live| self.origin.rendition_for(&live, named.resource).ok())
            .map(|rendition| rendition.contract.target_duration());
        let result = async {
            let live = self
                .origin
                .stream(&named.stream)
                .ok_or(DeliveryError::UnknownStream)?;
            let rendition = self.origin.rendition_for(&live, named.resource)?;
            let object = self
                .origin
                .media(
                    &named.stream,
                    named.resource,
                    self.hls.media_deadline(rendition.contract),
                )
                .await?;
            let target = object.contract.target_duration();
            DeliveryResponse::media(
                object.body,
                object.gzip,
                named.resource,
                rendition.media_kind,
                self.hls.media_reuse(Some(target)),
            )
        }
        .await;

        result.map_err(|error| DeliveryFailure {
            error,
            reuse: if matches!(
                error,
                DeliveryError::UnknownStream
                    | DeliveryError::UnknownRendition
                    | DeliveryError::UnknownResource
            ) {
                self.hls.missing_reuse(target, false)
            } else {
                Reuse::revalidate()
            },
        })
    }
}

impl Application for ViewerApplication {
    fn http_meters(&self, path: &str) -> Option<crate::observe::http::HttpMeters> {
        let stream = match parse_media_path(path) {
            Ok(Some(named)) => named.stream,
            _ => crate::delivery::hls::uri::parse_path(path).ok()?.stream,
        };
        self.origin.stream(&stream).map(|live| live.http_meters())
    }

    async fn serve<'a>(
        &'a self,
        path: &'a str,
        query: Option<&'a str>,
    ) -> Result<DeliveryResponse, DeliveryFailure> {
        match parse_media_path(path) {
            Ok(Some(named)) => self.serve_media(named).await,
            // HLS owns manifest recognition, including the distinction
            // between a malformed name and an absent resource.
            Ok(None) | Err(_) => self.hls.serve_path(path, query).await,
        }
    }
}

impl Node {
    pub fn new(
        mut config: NodeConfig,
        authenticator: Arc<dyn Authenticator>,
        events: Events,
        playback: Option<PlaybackSettings>,
    ) -> Result<Self, RuntimeError> {
        if config.maximum_sessions == 0 {
            return Err(RuntimeError::InvalidConfiguration(
                "maximum sessions must be nonzero",
            ));
        }
        if config.maintenance_interval.is_zero() {
            return Err(RuntimeError::InvalidConfiguration(
                "maintenance interval must be nonzero",
            ));
        }
        if config
            .metrics
            .token
            .as_ref()
            .is_some_and(super::metrics::MetricsToken::is_empty)
        {
            return Err(RuntimeError::InvalidConfiguration(
                "metrics token must not be empty",
            ));
        }
        // Rejected here rather than at the first cross-origin request, because
        // an unhonourable CORS policy fails inside the browser and leaves the
        // origin looking perfectly healthy.
        config
            .http
            .validate()
            .map_err(RuntimeError::InvalidConfiguration)?;

        config.hls.query_variables = playback.is_some();

        // There is one input policy for a publication. Keeping both transport
        // adapters and the session driver on the same value prevents bytes
        // that passed one boundary from being refused by the next under a
        // different supposedly process-wide limit.
        config.rtmp.input_limits = config.session.input;
        config.srt.input_limits = config.session.input;
        config.moq.input_limits = config.session.input;
        let store = StreamStore::try_new(config.store.clone())?.with_events(events.clone());
        let sessions = Registry::with_capacity(config.maximum_sessions);
        let meters = ProcessMeters::default();
        let recorder = config
            .record
            .as_ref()
            .map(|record| {
                crate::delivery::record::Recorder::start(record, events.clone(), meters.clone())
            })
            .transpose()
            .map_err(RuntimeError::Recording)?;
        let mut services = Services {
            authenticator,
            normalizers: Arc::new(PassThroughNormalizerFactory),
            muxers: Arc::new(PassThroughMuxerFactory),
            publishers: Arc::new(
                StorePublisherFactory::new(store.clone())
                    .with_events(events.clone())
                    .with_timing(
                        config.hls.timing.part_hold_back,
                        config.store.retention.retain,
                    ),
            ),
            sessions: sessions.clone(),
            meters: meters.clone(),
            events,
        };
        if let Some(recorder) = &recorder {
            services.publishers = Arc::new(crate::delivery::record::RecordingFactory {
                inner: services.publishers.clone(),
                recorder: recorder.clone(),
            });
        }
        let origin = Arc::new(Origin::new(store.clone()));
        let hls = Arc::new(HlsService::new(Arc::clone(&origin), config.hls.clone()));
        let application = Arc::new(ViewerApplication::new(
            Arc::clone(&origin),
            Arc::clone(&hls),
        ));
        let metrics = MetricsReader::new(
            meters,
            origin.meters().clone(),
            hls.meters().clone(),
            sessions,
            store.clone(),
        );

        Ok(Self {
            config,
            services,
            store,
            origin,
            hls,
            application,
            metrics,
            playback,
            recorder,
        })
    }

    /// Exports delivery counters for the configured hooks.
    ///
    /// Separate from [`Self::new`] because hooks are assembled by whoever owns
    /// the process's observer, which is also who decides whether there are any.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.metrics = self.metrics.with_hooks(hooks);
        self
    }

    pub fn services(&self) -> &Services {
        &self.services
    }

    pub fn store(&self) -> &StreamStore {
        &self.store
    }

    pub fn origin(&self) -> &Arc<Origin> {
        &self.origin
    }

    /// The viewer application this node's HTTP listeners serve.
    pub fn application(&self) -> Arc<ViewerApplication> {
        Arc::clone(&self.application)
    }

    pub fn metrics(&self) -> &MetricsReader {
        &self.metrics
    }

    /// Binds the MOQ listener, when an address asks for one.
    ///
    /// Apart from the others because it is the one listener that cannot start
    /// without a certificate: QUIC has no cleartext form, so an omitted
    /// `[moq] tls` is a refusal rather than a fallback to plaintext.
    fn bind_moq(&self) -> Result<Option<MoqListener>, RuntimeError> {
        let Some(address) = self.config.moq_address else {
            return Ok(None);
        };
        let settings = self
            .config
            .moq
            .tls
            .clone()
            .ok_or(RuntimeError::InvalidConfiguration(
                "a MOQ listener needs a certificate and key",
            ))?;
        let (tls, watch) = http::rotating_quic_server_config(
            settings,
            self.services.meters.clone(),
            self.services.events.clone(),
            Protocol::Moq,
        )?;
        let listener = MoqListener::bind(address, tls, watch, self.config.moq.clone())
            .map_err(|source| RuntimeError::BindMoq { address, source })?;
        Ok(Some(listener))
    }

    /// Runs both listeners and maintenance until `shutdown` resolves.
    pub async fn serve(
        mut self,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), RuntimeError> {
        let rtmp_listener =
            TcpListener::bind(self.config.rtmp_address)
                .await
                .map_err(|source| RuntimeError::BindRtmp {
                    address: self.config.rtmp_address,
                    source,
                })?;
        let srt_listener = SrtListener::bind(
            self.config.srt_address,
            self.config.srt.clone(),
            self.config.maximum_sessions,
        )
        .await
        .map_err(|source| RuntimeError::BindSrt {
            address: self.config.srt_address,
            source,
        })?;
        let moq_listener = self.bind_moq()?;
        // Each viewer listener binds on its own, so enabling TLS adds HTTPS
        // beside cleartext rather than moving it.
        let http_listener = match self.config.http_address {
            Some(address) => Some(bind_http(address).await?),
            None => None,
        };
        let https_listener = match self.config.https_address {
            Some(address) => Some(bind_http(address).await?),
            None => None,
        };
        // A metrics address equal to a viewer one is how an operator asks to
        // share that port, so it must not be bound a second time.
        let metrics_listener = match self.config.metrics.listen {
            Some(address)
                if Some(address) != self.config.http_address
                    && Some(address) != self.config.https_address =>
            {
                Some(bind_http(address).await?)
            }
            _ => None,
        };

        let events = self.services.events.clone();
        // Reported from the bound listeners rather than from configuration,
        // because with an ephemeral port the configured value is a zero and
        // the real one exists nowhere else.
        report_bound(&events, Protocol::Rtmp, rtmp_listener.local_addr());
        report_bound(&events, Protocol::Srt, Ok(srt_listener.local_address()));
        if let Some(listener) = &moq_listener {
            report_bound(
                &events,
                Protocol::Moq,
                listener
                    .local_address()
                    .map_err(|error| std::io::Error::other(error.to_string())),
            );
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let readiness = Readiness::default();
        let playback = match self.playback.take() {
            Some(settings) => {
                let gate = PlaybackGate::start(settings).await?;
                self.metrics = self.metrics.with_playback(gate.meters());
                Some(Arc::new(gate))
            }
            None => None,
        };
        let mut tasks = JoinSet::new();
        self.spawn_listeners(
            &mut tasks,
            rtmp_listener,
            srt_listener,
            moq_listener,
            http_listener,
            https_listener,
            metrics_listener,
            playback,
            &events,
            &readiness,
            stop_rx,
        )?;
        // All listeners and long-running tasks now exist. During shutdown this
        // flips before their drain begins, so load balancers stop adding work.
        readiness.mark_ready();

        tokio::pin!(shutdown);
        let (shutdown_requested, mut first_error) = tokio::select! {
            () = &mut shutdown => (true, None),
            joined = tasks.join_next() => (false, joined.and_then(task_error)),
        };

        let drain_deadline = tokio::time::Instant::now() + self.config.shutdown;
        readiness.mark_not_ready();

        if shutdown_requested {
            self.services.events.emit(NodeEvent::ShuttingDown);
        }
        self.services.sessions.stop_all(StopReason::Cancelled);
        let _ = stop_tx.send(true);
        while let Some(joined) = tasks.join_next().await {
            if first_error.is_none() {
                first_error = task_error(joined);
            }
        }

        if let Some(recorder) = &self.recorder {
            recorder
                .drain(drain_deadline.saturating_duration_since(tokio::time::Instant::now()))
                .await;
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// One budget each: a transport flooded with unauthenticated connections
    /// must not stop the other from admitting anyone.
    #[allow(clippy::too_many_arguments)]
    fn spawn_listeners(
        &self,
        tasks: &mut JoinSet<Result<(), RuntimeError>>,
        rtmp_listener: TcpListener,
        srt_listener: SrtListener,
        moq_listener: Option<MoqListener>,
        http_listener: Option<TcpListener>,
        https_listener: Option<TcpListener>,
        metrics_listener: Option<TcpListener>,
        playback: Option<Arc<PlaybackGate>>,
        events: &Events,
        readiness: &Readiness,
        stop_rx: watch::Receiver<bool>,
    ) -> Result<(), RuntimeError> {
        let maximum_pending = self.config.maximum_pending_publishers_per_listener();
        let session_config = Arc::new(self.config.session);
        tasks.spawn(run_ingest(
            RtmpListener {
                tcp: rtmp_listener,
                config: self.config.rtmp,
            },
            self.services.clone(),
            Arc::clone(&session_config),
            PendingPublishers::new(maximum_pending),
            stop_rx.clone(),
        ));
        let moq_session_config = moq_listener.is_some().then(|| Arc::clone(&session_config));
        tasks.spawn(run_ingest(
            srt_listener,
            self.services.clone(),
            session_config,
            PendingPublishers::new(maximum_pending),
            stop_rx.clone(),
        ));
        if let (Some(moq_listener), Some(session_config)) = (moq_listener, moq_session_config) {
            tasks.spawn(run_ingest(
                moq_listener,
                self.services.clone(),
                session_config,
                PendingPublishers::new(maximum_pending),
                stop_rx.clone(),
            ));
        }

        // Metrics are served on a viewer listener only when the operator gave
        // them that same address. Matching HTTP or HTTPS is one transport, not
        // both: a scrape on 8443 must not appear on 8080.
        let metrics = self.metrics_endpoint();
        let http_budget =
            http::HttpBudget::new(self.config.http.limits).with_meters(self.metrics.http_meters());

        if let Some(listener) = http_listener {
            report_bound(events, Protocol::Http, listener.local_addr());
            tasks.spawn(run_http(
                listener,
                Arc::clone(&self.application),
                self.config.http.clone(),
                http_budget.clone(),
                metrics
                    .clone()
                    .filter(|_| self.config.shares_metrics_with_http()),
                playback.clone(),
                readiness.clone(),
                stop_rx.clone(),
                self.config.shutdown,
            ));
        }

        // HTTPS is announced only once TLS is actually up. Announcing it
        // alongside the other listeners would put "HTTPS listening on …" in
        // the log immediately above the certificate error that stopped the
        // process from ever serving.
        if let Some(listener) = https_listener {
            let settings =
                self.config
                    .http
                    .tls
                    .clone()
                    .ok_or(RuntimeError::InvalidConfiguration(
                        "an HTTPS listener needs a certificate and key",
                    ))?;
            let listener = http::bind_tls(
                listener,
                settings,
                self.services.meters.clone(),
                events.clone(),
            )?;
            report_bound(events, Protocol::Https, listener.local_addr());
            tasks.spawn(run_http(
                listener,
                Arc::clone(&self.application),
                self.config.http.clone(),
                http_budget.clone(),
                metrics
                    .clone()
                    .filter(|_| self.config.shares_metrics_with_https()),
                playback.clone(),
                readiness.clone(),
                stop_rx.clone(),
                self.config.shutdown,
            ));
        }

        if let Some(listener) = metrics_listener {
            report_bound(events, Protocol::Http, listener.local_addr());
            tasks.spawn(run_http(
                listener,
                Arc::clone(&self.application),
                self.config.http.clone(),
                http_budget.clone(),
                self.metrics_endpoint(),
                playback,
                readiness.clone(),
                stop_rx.clone(),
                self.config.shutdown,
            ));
        }

        tasks.spawn(run_maintenance(
            self.store.clone(),
            Arc::clone(&self.hls),
            self.config.maintenance_interval,
            events.clone(),
            stop_rx,
        ));
        Ok(())
    }

    fn metrics_endpoint(&self) -> Option<MetricsEndpoint> {
        self.config
            .metrics
            .listen
            .map(|_| MetricsEndpoint::new(self.metrics.clone(), self.config.metrics.token.clone()))
    }
}

/// This node's default identity: its hostname, or a fixed fallback.
///
/// A fallback rather than a failure, because a node that cannot read its own
/// hostname is still a working origin — it just needs an operator to name it
/// before its metrics can be told from another's.
fn node_name() -> Arc<str> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .map_or_else(|| Arc::from("rushls"), |name| Arc::from(name.trim()))
}

/// Binds one viewer-facing or metrics listener.
async fn bind_http(address: SocketAddr) -> Result<TcpListener, RuntimeError> {
    TcpListener::bind(address)
        .await
        .map_err(|source| RuntimeError::BindHttp { address, source })
}

/// What one `accept` produced.
///
/// Three outcomes rather than a `Result`, because "this peer went away" and
/// "this listener is finished" are not the same event and only one of them
/// stops the node.
enum Accepted<C> {
    Connection(C),
    /// The peer never got as far as being a publisher. Reported and forgotten;
    /// how often it happens is up to whoever is connecting.
    Refused(String),
    /// The listener itself can no longer accept, which fails the process.
    Stopped(RuntimeError),
}

/// One ingest transport, reduced to what the accept loop needs to know.
///
/// The loop below is the same for every protocol — reserve, accept, spawn,
/// drain — and the two implementations differ only in where the handshake
/// happens. SRT completes its own inside `accept`; RTMP hands back a bare
/// socket and negotiates on the connection's task, so a peer that stalls
/// mid-handshake cannot hold up the next one. That distinction is the reason
/// [`Self::handshake`] exists as a separate step rather than being folded into
/// accepting.
trait IngestListener: Send + 'static {
    /// What `accept` yields: everything the connection's own task will need,
    /// since it cannot borrow the listener.
    type Connection: Send + 'static;

    const PROTOCOL: Protocol;

    fn accept(&mut self) -> impl Future<Output = Accepted<Self::Connection>> + Send;

    /// Negotiates on the connection's own task.
    fn handshake(
        connection: Self::Connection,
    ) -> impl Future<Output = Result<Box<dyn PendingPublish>, TransportError>> + Send;
}

impl IngestListener for MoqListener {
    type Connection = (web_transport_quinn::quinn::Incoming, MoqConfig);

    const PROTOCOL: Protocol = Protocol::Moq;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match MoqListener::accept(self).await {
            Some(request) => Accepted::Connection((request, self.config())),
            None => Accepted::Stopped(RuntimeError::MoqStopped),
        }
    }

    async fn handshake(
        (request, config): Self::Connection,
    ) -> Result<Box<dyn PendingPublish>, TransportError> {
        MoqPendingPublish::handshake(request, config)
            .await
            .map(|pending| Box::new(pending) as Box<dyn PendingPublish>)
    }
}

impl IngestListener for SrtListener {
    type Connection = SrtPendingPublish;

    const PROTOCOL: Protocol = Protocol::Srt;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match SrtListener::accept(self).await {
            Some(Ok(pending)) => Accepted::Connection(pending),
            Some(Err(error)) => Accepted::Refused(error.to_string()),
            None => Accepted::Stopped(RuntimeError::SrtStopped),
        }
    }

    /// Already negotiated: rsrt completes its handshake inside `accept`.
    fn handshake(
        connection: Self::Connection,
    ) -> impl Future<Output = Result<Box<dyn PendingPublish>, TransportError>> {
        std::future::ready(Ok(Box::new(connection) as Box<dyn PendingPublish>))
    }
}

/// A bound TCP listener and the RTMP settings its handshakes negotiate under.
struct RtmpListener {
    tcp: TcpListener,
    config: RtmpConfig,
}

impl IngestListener for RtmpListener {
    type Connection = (tokio::net::TcpStream, RtmpConfig);

    const PROTOCOL: Protocol = Protocol::Rtmp;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match self.tcp.accept().await {
            Ok((stream, _)) => Accepted::Connection((stream, self.config)),
            Err(error) => Accepted::Stopped(RuntimeError::Rtmp(error)),
        }
    }

    async fn handshake(
        (stream, config): Self::Connection,
    ) -> Result<Box<dyn PendingPublish>, TransportError> {
        RtmpPendingPublish::handshake_tcp(stream, config)
            .await
            .map(|pending| Box::new(pending) as Box<dyn PendingPublish>)
    }
}

/// Accepts publishers until `stop`, then lets what is in flight finish.
async fn run_ingest<L: IngestListener>(
    mut listener: L,
    services: Services,
    session_config: Arc<SessionConfig>,
    pending_publishers: PendingPublishers,
    mut stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut connections = JoinSet::new();
    loop {
        // Capacity is reserved *before* accepting, so a surplus of connections
        // waits in the listener's backlog instead of becoming tasks that
        // nothing has authenticated. Accepting and then closing would give an
        // attacker cheap connection churn and give a well-behaved encoder
        // nothing to retry against.
        let slot = tokio::select! {
            biased;

            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
                continue;
            }

            completed = connections.join_next(), if !connections.is_empty() => {
                report_panic::<L>(&services, completed);
                continue;
            }

            slot = pending_publishers.reserve() => slot,
        };

        tokio::select! {
            biased;

            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }

            accepted = listener.accept() => {
                let connection = match accepted {
                    Accepted::Connection(connection) => connection,
                    Accepted::Refused(reason) => {
                        services.events.emit(NodeEvent::PublisherHandshakeFailed {
                            protocol: L::PROTOCOL,
                            reason,
                        });
                        continue;
                    }
                    Accepted::Stopped(error) => return Err(error),
                };
                let services = services.clone();
                let session_config = Arc::clone(&session_config);
                connections.spawn(async move {
                    let pending = match L::handshake(connection).await {
                        Ok(pending) => pending,
                        Err(error) => {
                            services.events.emit(NodeEvent::PublisherHandshakeFailed {
                                protocol: L::PROTOCOL,
                                reason: error.to_string(),
                            });
                            return;
                        }
                    };
                    if let Err(error) =
                        run_session(pending, &services, session_config.as_ref(), slot).await
                    {
                        services.events.emit(NodeEvent::PublisherSessionFailed {
                            protocol: L::PROTOCOL,
                            reason: error.to_string(),
                        });
                    }
                });
            }
        }
    }

    // Stops accepting before the drain rather than after it, so an encoder
    // reconnecting into a node that is going away is refused immediately
    // instead of being accepted onto a listener about to disappear.
    drop(listener);
    while let Some(completed) = connections.join_next().await {
        report_panic::<L>(&services, Some(completed));
    }
    Ok(())
}

/// A connection task that panicked is always a defect: a session reports its
/// own failures, so reaching this means one never got the chance.
fn report_panic<L: IngestListener>(services: &Services, completed: Option<Result<(), JoinError>>) {
    if let Some(Err(error)) = completed {
        services.events.emit(NodeEvent::ConnectionTaskPanicked {
            protocol: L::PROTOCOL,
            reason: error.to_string(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_http<L>(
    listener: L,
    application: Arc<ViewerApplication>,
    config: HttpConfig,
    budget: http::HttpBudget,
    metrics: Option<MetricsEndpoint>,
    playback: Option<Arc<PlaybackGate>>,
    readiness: Readiness,
    stop: watch::Receiver<bool>,
    shutdown: Duration,
) -> Result<(), RuntimeError>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    let served = http::serve_with_budget(
        listener,
        application,
        config,
        metrics,
        playback,
        readiness,
        budget,
        wait_for_stop(stop.clone()),
    );
    tokio::pin!(served);
    // The drain is bounded rather than open-ended. A blocking playlist reload
    // is deliberately parked for up to three target durations, so without a
    // bound a restart could outlive its orchestrator's grace period and be
    // hard-killed mid-drain, which is the outcome the graceful path exists to
    // avoid. Viewers still holding a parked request are dropped at the
    // deadline; they reload.
    // The bound covers only the drain after the stop signal.
    tokio::select! {
        biased;
        result = &mut served => result.map_err(RuntimeError::Http),
        () = wait_for_stop(stop) => {
            match tokio::time::timeout(shutdown, served).await {
                Ok(result) => result.map_err(RuntimeError::Http),
                Err(_) => Ok(()),
            }
        }
    }
}

/// Reports a listener's real address, or the failure to learn it.
///
/// A listener that bound but cannot name itself is not worth failing startup
/// over — it serves perfectly well — but it is worth saying out loud, because
/// every subsequent log line about that port will be missing.
fn report_bound(events: &Events, protocol: Protocol, address: std::io::Result<SocketAddr>) {
    match address {
        Ok(address) => events.emit(NodeEvent::ListenerBound { protocol, address }),
        Err(error) => events.emit(NodeEvent::ListenerAddressUnavailable {
            protocol,
            reason: error.to_string(),
        }),
    }
}

async fn run_maintenance(
    store: StreamStore,
    origin: Arc<HlsService>,
    interval: Duration,
    events: Events,
    mut stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            _ = ticks.tick() => {
                let reachability = store.maintain();
                origin.remove_streams(&reachability.retired);
                for stream in reachability.retired {
                    events.stream(stream, StreamEvent::Retired);
                }
            }
        }
    }
}

async fn wait_for_stop(mut stop: watch::Receiver<bool>) {
    if *stop.borrow() {
        return;
    }
    while stop.changed().await.is_ok() {
        if *stop.borrow() {
            return;
        }
    }
}

fn task_error(joined: Result<Result<(), RuntimeError>, JoinError>) -> Option<RuntimeError> {
    match joined {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(error) => Some(RuntimeError::Task(error)),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::{OpenStreamAuthenticator, StreamPolicy},
        observe::{EventObserver, Events, NodeEvent, SessionEvent},
        server::metrics::MetricsToken,
    };

    use super::*;

    fn node(config: NodeConfig) -> Result<Node, RuntimeError> {
        node_with_events(config, Events::default())
    }

    fn node_with_events(config: NodeConfig, events: Events) -> Result<Node, RuntimeError> {
        Node::new(
            config,
            Arc::new(OpenStreamAuthenticator::new(StreamPolicy::permissive())),
            events,
            None,
        )
    }

    #[test]
    fn one_input_limit_is_shared_by_transport_and_session() {
        let mut config = NodeConfig::default();
        config.session.input.maximum_packets_per_batch = 17;
        config.rtmp.input_limits.maximum_packets_per_batch = 99;
        config.srt.input_limits.maximum_packets_per_batch = 98;
        config.moq.input_limits.maximum_packets_per_batch = 97;

        let node = node(config).expect("configuration is valid");

        assert_eq!(node.config.rtmp.input_limits.maximum_packets_per_batch, 17);
        assert_eq!(node.config.srt.input_limits.maximum_packets_per_batch, 17);
        assert_eq!(node.config.moq.input_limits.maximum_packets_per_batch, 17);
    }

    #[test]
    fn zero_process_limits_are_rejected() {
        assert!(matches!(
            node(NodeConfig {
                maximum_sessions: 0,
                ..NodeConfig::default()
            }),
            Err(RuntimeError::InvalidConfiguration(_))
        ));
        assert!(matches!(
            node(NodeConfig {
                maintenance_interval: Duration::ZERO,
                ..NodeConfig::default()
            }),
            Err(RuntimeError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn an_empty_metrics_token_is_rejected_by_programmatic_configuration() {
        assert!(matches!(
            node(NodeConfig {
                metrics: MetricsConfig {
                    token: Some(MetricsToken::new("")),
                    ..MetricsConfig::default()
                },
                ..NodeConfig::default()
            }),
            Err(RuntimeError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn metrics_attach_to_the_one_viewer_listener_whose_address_was_given() {
        let http = "127.0.0.1:8080".parse().expect("constant address is valid");
        let https = "127.0.0.1:8443".parse().expect("constant address is valid");
        let dedicated = "127.0.0.1:9090".parse().expect("constant address is valid");
        let mut config = NodeConfig {
            http_address: Some(http),
            https_address: Some(https),
            ..NodeConfig::default()
        };

        config.metrics.listen = Some(https);
        assert!(
            !config.shares_metrics_with_http(),
            "sharing HTTPS must not mount /metrics on cleartext"
        );
        assert!(config.shares_metrics_with_https());

        config.metrics.listen = Some(http);
        assert!(config.shares_metrics_with_http());
        assert!(!config.shares_metrics_with_https());

        config.metrics.listen = Some(dedicated);
        assert!(
            !config.shares_metrics_with_http() && !config.shares_metrics_with_https(),
            "a dedicated scrape port is its own listener, always cleartext"
        );
    }

    #[tokio::test]
    async fn an_immediate_shutdown_stops_every_runtime_task() -> Result<(), RuntimeError> {
        #[derive(Default)]
        struct Recorder(parking_lot::Mutex<Vec<NodeEvent>>);

        impl EventObserver for Recorder {
            fn observe(&self, _session: crate::domain::SessionId, _event: SessionEvent) {}

            fn observe_node(&self, event: NodeEvent) {
                self.0.lock().push(event);
            }
        }

        let directory = crate::server::http::fixtures::scratch("moq-runtime");
        let (tls, _) = crate::server::http::fixtures::write_pair(&directory, "origin.test");
        let recorder = Arc::new(Recorder::default());
        let mut config = NodeConfig {
            rtmp_address: "127.0.0.1:0".parse().expect("constant is valid"),
            srt_address: "127.0.0.1:0".parse().expect("constant is valid"),
            moq_address: Some("127.0.0.1:0".parse().expect("constant is valid")),
            http_address: Some("127.0.0.1:0".parse().expect("constant is valid")),
            ..NodeConfig::default()
        };
        config.moq.tls = Some(tls);
        let node = node_with_events(
            config,
            Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>),
        )?;

        node.serve(async {}).await?;
        let events = recorder.0.lock().clone();
        assert!(
            events.iter().any(|event| matches!(
                event,
                NodeEvent::ListenerBound {
                    protocol: Protocol::Moq,
                    ..
                }
            )),
            "MOQ must bind when listen is on: {events:?}"
        );
        assert!(matches!(events.last(), Some(NodeEvent::ShuttingDown)));
        Ok(())
    }

    #[tokio::test]
    async fn an_occupied_rtmp_port_fails_with_the_address_attached() -> Result<(), RuntimeError> {
        // Same class of failure as routmp's host-port collision: whatever the
        // cause, startup must name the port instead of exiting opaquely.
        let predecessor = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture binds");
        let occupied = predecessor.local_addr().expect("fixture has an address");
        let config = NodeConfig {
            rtmp_address: occupied,
            srt_address: "127.0.0.1:0".parse().expect("constant is valid"),
            ..NodeConfig::default()
        };
        let node = node(config)?;
        // RTMP binds first in serve, so nothing else is touched.
        let error = node
            .serve(async {})
            .await
            .expect_err("an occupied RTMP port must fail startup");
        match &error {
            RuntimeError::BindRtmp { address, source } => {
                assert_eq!(*address, occupied);
                assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);
            }
            other => panic!("must be BindRtmp, got: {other}"),
        }
        assert!(
            error.to_string().contains(&occupied.to_string()),
            "the operator-facing error must name the port: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_occupied_http_port_fails_with_the_address_attached() {
        let predecessor = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture binds");
        let occupied = predecessor.local_addr().expect("fixture has an address");
        let error = bind_http(occupied)
            .await
            .expect_err("an occupied HTTP port must fail");
        match &error {
            RuntimeError::BindHttp { address, source } => {
                assert_eq!(*address, occupied);
                assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);
            }
            other => panic!("must be BindHttp, got: {other}"),
        }
        assert!(
            error.to_string().contains(&occupied.to_string()),
            "the operator-facing error must name the port: {error}"
        );
    }
}
