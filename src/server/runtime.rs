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
    session::{
        PendingPublishers, PublishersPerAddress, Registry, Services, SessionConfig, StopReason,
        run_session,
    },
    source::{
        PendingPublish, TransportError,
        transport::{
            moq::{MoqConfig, MoqConnection, MoqListener, MoqPendingPublish},
            proxy,
            rtmp::{RtmpConfig, RtmpPendingPublish},
            srt::{SrtConfig, SrtListener, SrtPendingPublish},
        },
    },
};

use super::{
    http::{
        self, Application, HttpConfig, PlaybackGate, PlaybackSettings, PlaybackStartError,
        Readiness, TlsError, TlsSettings,
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
    /// Expect a PROXY protocol header on every RTMP connection, naming the
    /// client behind a load balancer or TLS terminator.
    pub rtmp_proxy_protocol: bool,
    /// RTMP over TLS terminated by this node. `None` leaves it off, which is
    /// the compiled default so a process can boot without certificates.
    pub rtmps_address: Option<SocketAddr>,
    /// Expect a PROXY protocol header before the TLS handshake on every RTMPS
    /// connection, for a TCP load balancer passing TLS through untouched.
    pub rtmps_proxy_protocol: bool,
    /// Certificate and TLS versions for RTMPS, from `[tls]`.
    pub rtmps_tls: Option<TlsSettings>,
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
    /// Publishers one client address may hold, pending and admitted, across
    /// every ingest listener. `None` counts nothing.
    pub maximum_publishers_per_address: Option<std::num::NonZeroUsize>,
    /// `memory.total`: node-wide memory committed across publisher and stream
    /// budgets, or `None` for no ceiling.
    pub memory_total: Option<usize>,
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
            rtmp_proxy_protocol: false,
            rtmps_address: None,
            rtmps_proxy_protocol: false,
            rtmps_tls: None,
            srt_address: "0.0.0.0:9000".parse().expect("constant address is valid"),
            moq_address: None,
            http_address: Some("0.0.0.0:8080".parse().expect("constant address is valid")),
            https_address: None,
            maintenance_interval: Duration::from_secs(1),
            maximum_sessions: 256,
            maximum_publishers_per_address: None,
            memory_total: None,
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
    #[error("could not bind RTMPS at {address}: {source}")]
    BindRtmps {
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
        // One ledger shared by the store and the registry, so stream and
        // publisher budgets commit against the same total.
        let ledger = config.memory_total.map(crate::domain::MemoryLedger::new);
        config.store.memory.clone_from(&ledger);
        let store = StreamStore::try_new(config.store.clone())?.with_events(events.clone());
        let sessions = Registry::with_memory(
            config.maximum_sessions,
            ledger.zip(config.session.memory_per_publisher),
        );
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
    /// `ingest.moq` without `[tls]` is a refusal rather than a fallback to plaintext.
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

    /// Binds every configured ingest listener and reports where each listens.
    async fn bind_ingest(&self) -> Result<IngestListeners, RuntimeError> {
        let rtmp = TcpListener::bind(self.config.rtmp_address)
            .await
            .map_err(|source| RuntimeError::BindRtmp {
                address: self.config.rtmp_address,
                source,
            })?;
        let srt = SrtListener::bind(
            self.config.srt_address,
            self.config.srt.clone(),
            self.config.maximum_sessions,
        )
        .await
        .map_err(|source| RuntimeError::BindSrt {
            address: self.config.srt_address,
            source,
        })?;
        let moq = self.bind_moq()?;
        let rtmps = self.bind_rtmps().await?;

        let events = &self.services.events;
        // Reported from the bound listeners rather than from configuration,
        // because with an ephemeral port the configured value is a zero and
        // the real one exists nowhere else.
        report_bound(events, Protocol::Rtmp, rtmp.local_addr());
        if let Some(listener) = &rtmps {
            report_bound(events, Protocol::Rtmps, listener.tcp.local_addr());
        }
        report_bound(events, Protocol::Srt, Ok(srt.local_address()));
        if let Some(listener) = &moq {
            report_bound(
                events,
                Protocol::Moq,
                listener
                    .local_address()
                    .map_err(|error| std::io::Error::other(error.to_string())),
            );
        }
        Ok(IngestListeners {
            rtmp,
            rtmps,
            srt,
            moq,
        })
    }

    /// Binds the RTMPS listener, when an address asks for one.
    ///
    /// Like MOQ, it cannot start without a certificate, so a missing one is a
    /// refusal rather than a fallback to cleartext on the RTMPS port.
    async fn bind_rtmps(&self) -> Result<Option<RtmpsListener>, RuntimeError> {
        let Some(address) = self.config.rtmps_address else {
            return Ok(None);
        };
        let settings = self
            .config
            .rtmps_tls
            .clone()
            .ok_or(RuntimeError::InvalidConfiguration(
                "an RTMPS listener needs a certificate and key",
            ))?;
        let (tls, watch) = http::rotating_ingest_server_config(
            settings,
            self.services.meters.clone(),
            self.services.events.clone(),
            Protocol::Rtmps,
        )?;
        let tcp = TcpListener::bind(address)
            .await
            .map_err(|source| RuntimeError::BindRtmps { address, source })?;
        Ok(Some(RtmpsListener {
            tcp,
            acceptor: tokio_rustls::TlsAcceptor::from(tls),
            config: RtmpConfig {
                protocol: crate::domain::IngestProtocol::Rtmps,
                ..self.config.rtmp
            },
            proxy_protocol: self.config.rtmps_proxy_protocol,
            _watch: watch,
        }))
    }

    /// Runs both listeners and maintenance until `shutdown` resolves.
    pub async fn serve(
        mut self,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), RuntimeError> {
        let ingest = self.bind_ingest().await?;
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
            ingest,
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
        ingest: IngestListeners,
        http_listener: Option<TcpListener>,
        https_listener: Option<TcpListener>,
        metrics_listener: Option<TcpListener>,
        playback: Option<Arc<PlaybackGate>>,
        events: &Events,
        readiness: &Readiness,
        stop_rx: watch::Receiver<bool>,
    ) -> Result<(), RuntimeError> {
        self.spawn_ingest(tasks, ingest, &stop_rx);

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

    /// One accept loop per ingest listener, each with its own pending budget
    /// and all sharing one per-address count.
    fn spawn_ingest(
        &self,
        tasks: &mut JoinSet<Result<(), RuntimeError>>,
        ingest: IngestListeners,
        stop_rx: &watch::Receiver<bool>,
    ) {
        let maximum_pending = self.config.maximum_pending_publishers_per_listener();
        let session_config = Arc::new(self.config.session);
        // One count shared by every listener: the limit is per client.
        let per_address = self
            .config
            .maximum_publishers_per_address
            .map(PublishersPerAddress::new);
        tasks.spawn(run_ingest(
            RtmpListener {
                tcp: ingest.rtmp,
                config: self.config.rtmp,
                proxy_protocol: self.config.rtmp_proxy_protocol,
            },
            self.services.clone(),
            Arc::clone(&session_config),
            PendingPublishers::new(maximum_pending),
            per_address.clone(),
            stop_rx.clone(),
        ));
        if let Some(rtmps) = ingest.rtmps {
            tasks.spawn(run_ingest(
                rtmps,
                self.services.clone(),
                Arc::clone(&session_config),
                PendingPublishers::new(maximum_pending),
                per_address.clone(),
                stop_rx.clone(),
            ));
        }
        let moq_session_config = ingest.moq.is_some().then(|| Arc::clone(&session_config));
        tasks.spawn(run_ingest(
            ingest.srt,
            self.services.clone(),
            session_config,
            PendingPublishers::new(maximum_pending),
            per_address.clone(),
            stop_rx.clone(),
        ));
        if let (Some(moq_listener), Some(session_config)) = (ingest.moq, moq_session_config) {
            tasks.spawn(run_ingest(
                moq_listener,
                self.services.clone(),
                session_config,
                PendingPublishers::new(maximum_pending),
                per_address,
                stop_rx.clone(),
            ));
        }
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

/// Every bound ingest listener, handed to the accept loops as one.
struct IngestListeners {
    rtmp: TcpListener,
    rtmps: Option<RtmpsListener>,
    srt: SrtListener,
    moq: Option<MoqListener>,
}

/// Binds one viewer-facing or metrics listener.
async fn bind_http(address: SocketAddr) -> Result<TcpListener, RuntimeError> {
    TcpListener::bind(address)
        .await
        .map_err(|source| RuntimeError::BindHttp { address, source })
}

/// What one `accept` produced.
///
/// Four outcomes rather than a `Result`, because "this peer went away", "the
/// listener could not accept just now", and "this listener is finished" are
/// different events and only the last one stops the node.
enum Accepted<C> {
    Connection(C),
    /// The peer never got as far as being a publisher. Reported and forgotten;
    /// how often it happens is up to whoever is connecting.
    Refused(String),
    /// The socket's `accept` failed, but the listener is still usable. Covers
    /// descriptor exhaustion (`EMFILE`, `ENFILE`) and a peer that reset while
    /// still queued; neither is a reason to stop ingest for every publisher.
    Failed(String),
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
    /// A connection whose client address is established.
    type Identified: Send + 'static;

    const PROTOCOL: Protocol;

    fn accept(&mut self) -> impl Future<Output = Accepted<Self::Connection>> + Send;

    /// Establishes the client address, on the connection's own task.
    ///
    /// The address must be one the peer cannot simply claim, because
    /// `limits.publishers_per_address` counts it: a spoofed address could
    /// otherwise use up a real client's allowance. TCP and SRT have proved
    /// theirs by the time they are accepted; QUIC proves it by completing
    /// its handshake; a PROXY header is trusted because only the proxy can
    /// reach a listener that requires one.
    fn identify(
        connection: Self::Connection,
    ) -> impl Future<Output = Result<(SocketAddr, Self::Identified), TransportError>> + Send;

    /// Negotiates on the connection's own task.
    fn handshake(
        connection: Self::Identified,
        client: SocketAddr,
    ) -> impl Future<Output = Result<Box<dyn PendingPublish>, TransportError>> + Send;
}

impl IngestListener for MoqListener {
    type Connection = (web_transport_quinn::quinn::Incoming, MoqConfig);
    type Identified = MoqConnection;

    const PROTOCOL: Protocol = Protocol::Moq;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match MoqListener::accept(self).await {
            Some(request) => Accepted::Connection((request, self.config())),
            None => Accepted::Stopped(RuntimeError::MoqStopped),
        }
    }

    async fn identify(
        (request, config): Self::Connection,
    ) -> Result<(SocketAddr, Self::Identified), TransportError> {
        let connection = MoqPendingPublish::connect(request, config).await?;
        Ok((connection.remote_address(), connection))
    }

    async fn handshake(
        connection: Self::Identified,
        _client: SocketAddr,
    ) -> Result<Box<dyn PendingPublish>, TransportError> {
        connection
            .handshake()
            .await
            .map(|pending| Box::new(pending) as Box<dyn PendingPublish>)
    }
}

impl IngestListener for SrtListener {
    type Connection = SrtPendingPublish;
    type Identified = SrtPendingPublish;

    const PROTOCOL: Protocol = Protocol::Srt;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match SrtListener::accept(self).await {
            Some(Ok(pending)) => Accepted::Connection(pending),
            Some(Err(error)) => Accepted::Refused(error.to_string()),
            None => Accepted::Stopped(RuntimeError::SrtStopped),
        }
    }

    /// rsrt's handshake includes a cookie exchange, so the address is proven.
    fn identify(
        connection: Self::Connection,
    ) -> impl Future<Output = Result<(SocketAddr, Self::Identified), TransportError>> {
        std::future::ready(
            connection
                .publish_request()
                .map(|request| (request.client.remote_address, connection)),
        )
    }

    /// Already negotiated: rsrt completes its handshake inside `accept`.
    fn handshake(
        connection: Self::Identified,
        _client: SocketAddr,
    ) -> impl Future<Output = Result<Box<dyn PendingPublish>, TransportError>> {
        std::future::ready(Ok(Box::new(connection) as Box<dyn PendingPublish>))
    }
}

/// A bound TCP listener and the RTMP settings its handshakes negotiate under.
struct RtmpListener {
    tcp: TcpListener,
    config: RtmpConfig,
    proxy_protocol: bool,
}

impl IngestListener for RtmpListener {
    type Connection = (tokio::net::TcpStream, SocketAddr, RtmpConfig, bool);
    type Identified = (tokio::net::TcpStream, RtmpConfig);

    const PROTOCOL: Protocol = Protocol::Rtmp;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match self.tcp.accept().await {
            Ok((stream, peer)) => {
                Accepted::Connection((stream, peer, self.config, self.proxy_protocol))
            }
            Err(error) => Accepted::Failed(error.to_string()),
        }
    }

    async fn identify(
        (mut stream, peer, config, proxy_protocol): Self::Connection,
    ) -> Result<(SocketAddr, Self::Identified), TransportError> {
        let client = tcp_client(&mut stream, peer, &config, proxy_protocol).await?;
        Ok((client, (stream, config)))
    }

    async fn handshake(
        (stream, config): Self::Identified,
        client: SocketAddr,
    ) -> Result<Box<dyn PendingPublish>, TransportError> {
        RtmpPendingPublish::handshake(stream, client, config)
            .await
            .map(|pending| Box::new(pending) as Box<dyn PendingPublish>)
    }
}

/// The deadline for everything a TCP ingest peer does before its first
/// publish command can arrive: a PROXY header, a TLS handshake.
///
/// Each is as unauthenticated as the RTMP handshake itself, so none gets more
/// patience than it.
fn tcp_handshake_deadline(config: &RtmpConfig) -> Duration {
    config
        .timeouts
        .handshake_read
        .map_or(config.maximum_publish_wait, |read| {
            read.min(config.maximum_publish_wait)
        })
}

/// The client behind a TCP ingest connection: the socket's peer, or the
/// address a required PROXY header names.
async fn tcp_client(
    stream: &mut tokio::net::TcpStream,
    peer: SocketAddr,
    config: &RtmpConfig,
    proxy_protocol: bool,
) -> Result<SocketAddr, TransportError> {
    if !proxy_protocol {
        return Ok(peer);
    }
    // Under the handshake's own deadline: a proxy that connects and sends
    // nothing is as unauthenticated as any other silent peer.
    tokio::time::timeout(
        tcp_handshake_deadline(config),
        proxy::read_header(stream, peer),
    )
    .await
    .map_err(|_| TransportError::Handshake("no PROXY protocol header before the deadline".into()))?
    .map_err(|reason| TransportError::Handshake(reason.into()))
}

/// RTMP inside TLS that this node terminates itself.
///
/// Not built on [`http::TlsListener`], which completes handshakes before
/// yielding a connection. Ingest must reserve a pending-publisher slot and
/// count the client address first, so the TLS handshake runs on the
/// connection's own task like the RTMP handshake after it. A PROXY header, when
/// required, precedes TLS: a TCP load balancer passing TLS through prepends it
/// in cleartext.
struct RtmpsListener {
    tcp: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    config: RtmpConfig,
    proxy_protocol: bool,
    /// Dropping it would silently stop certificate reloads.
    _watch: rushls_tls::CertificateWatch,
}

impl IngestListener for RtmpsListener {
    type Connection = (
        tokio::net::TcpStream,
        SocketAddr,
        RtmpConfig,
        bool,
        tokio_rustls::TlsAcceptor,
    );
    type Identified = (tokio::net::TcpStream, RtmpConfig, tokio_rustls::TlsAcceptor);

    const PROTOCOL: Protocol = Protocol::Rtmps;

    async fn accept(&mut self) -> Accepted<Self::Connection> {
        match self.tcp.accept().await {
            Ok((stream, peer)) => Accepted::Connection((
                stream,
                peer,
                self.config,
                self.proxy_protocol,
                self.acceptor.clone(),
            )),
            Err(error) => Accepted::Failed(error.to_string()),
        }
    }

    async fn identify(
        (mut stream, peer, config, proxy_protocol, acceptor): Self::Connection,
    ) -> Result<(SocketAddr, Self::Identified), TransportError> {
        let client = tcp_client(&mut stream, peer, &config, proxy_protocol).await?;
        Ok((client, (stream, config, acceptor)))
    }

    /// TLS first, after the per-address count, so a client over its limit
    /// never costs a handshake.
    async fn handshake(
        (stream, config, acceptor): Self::Identified,
        client: SocketAddr,
    ) -> Result<Box<dyn PendingPublish>, TransportError> {
        let started = tokio::time::Instant::now();
        let stream = tokio::time::timeout(tcp_handshake_deadline(&config), acceptor.accept(stream))
            .await
            .map_err(|_| TransportError::Handshake("TLS handshake did not finish in time".into()))?
            .map_err(|error| {
                TransportError::Handshake(format!("TLS handshake failed: {error}").into())
            })?;
        // The RTMP handshake gets what is left of the pre-publish allowance,
        // so TLS cannot stretch the unauthenticated phase past it.
        let config = RtmpConfig {
            maximum_publish_wait: config
                .maximum_publish_wait
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(1)),
            ..config
        };
        RtmpPendingPublish::handshake(stream, client, config)
            .await
            .map(|pending| Box::new(pending) as Box<dyn PendingPublish>)
    }
}

/// Handshakes a connection whose address is established, holding its
/// per-address permit for the rest of the session.
async fn run_connection<L: IngestListener>(
    connection: L::Connection,
    services: Services,
    session_config: Arc<SessionConfig>,
    per_address: Option<PublishersPerAddress>,
    slot: crate::session::PendingPermit,
) {
    let refused = |reason: String| {
        services.events.emit(NodeEvent::PublisherHandshakeFailed {
            protocol: L::PROTOCOL,
            reason,
        });
    };
    let (client, identified) = match L::identify(connection).await {
        Ok(identified) => identified,
        Err(error) => return refused(error.to_string()),
    };
    // Dropped when this task ends, so it spans admission and the session.
    let _permit = match per_address
        .map(|limit| limit.try_acquire(client.ip()))
        .transpose()
    {
        Ok(permit) => permit,
        Err(full) => {
            services.meters.publisher_address_limited();
            services.events.emit(NodeEvent::PublisherAddressLimited {
                protocol: L::PROTOCOL,
                address: full.address.to_string(),
                maximum: full.maximum,
            });
            return;
        }
    };
    let pending = match L::handshake(identified, client).await {
        Ok(pending) => pending,
        Err(error) => return refused(error.to_string()),
    };
    if let Err(error) = run_session(pending, &services, session_config.as_ref(), slot).await {
        services.events.emit(NodeEvent::PublisherSessionFailed {
            protocol: L::PROTOCOL,
            reason: error.to_string(),
        });
    }
}

/// Accepts publishers until `stop`, then lets what is in flight finish.
/// How long an ingest listener waits after a failed `accept` before retrying.
/// Matches the TLS listener in `rushls-tls`.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);

async fn run_ingest<L: IngestListener>(
    mut listener: L,
    services: Services,
    session_config: Arc<SessionConfig>,
    pending_publishers: PendingPublishers,
    per_address: Option<PublishersPerAddress>,
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
                    Accepted::Failed(reason) => {
                        services.events.emit(NodeEvent::ListenerAcceptFailed {
                            protocol: L::PROTOCOL,
                            reason,
                        });
                        // Retrying at once would spin a core while descriptors
                        // stay exhausted; the pause gives sessions time to end.
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                        continue;
                    }
                    Accepted::Stopped(error) => return Err(error),
                };
                connections.spawn(run_connection::<L>(
                    connection,
                    services.clone(),
                    Arc::clone(&session_config),
                    per_address.clone(),
                    slot,
                ));
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

    /// Keeps every node event, for tests that assert on what was reported.
    #[derive(Default)]
    struct Recorder(parking_lot::Mutex<Vec<NodeEvent>>);

    impl EventObserver for Recorder {
        fn observe(&self, _session: crate::domain::SessionId, _event: SessionEvent) {}

        fn observe_node(&self, event: NodeEvent) {
            self.0.lock().push(event);
        }
    }

    impl Recorder {
        /// Waits for an event, since connections are handled on their own tasks.
        async fn find<T>(&self, pick: impl Fn(&NodeEvent) -> Option<T>) -> Option<T> {
            for _ in 0..200 {
                if let Some(found) = self.0.lock().iter().find_map(&pick) {
                    return Some(found);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            None
        }
    }

    /// A failed `accept` is reported and retried; only a stopped listener ends
    /// ingest. Descriptor exhaustion used to take the whole node down.
    #[tokio::test]
    async fn a_failed_accept_is_reported_and_retried() -> Result<(), RuntimeError> {
        /// Replays scripted outcomes, then reports the listener as stopped.
        struct Scripted(std::collections::VecDeque<Accepted<()>>);

        impl IngestListener for Scripted {
            type Connection = ();
            type Identified = ();

            const PROTOCOL: Protocol = Protocol::Rtmp;

            fn accept(&mut self) -> impl Future<Output = Accepted<()>> + Send {
                std::future::ready(
                    self.0
                        .pop_front()
                        .unwrap_or(Accepted::Stopped(RuntimeError::SrtStopped)),
                )
            }

            fn identify(
                (): (),
            ) -> impl Future<Output = Result<(SocketAddr, ()), TransportError>> + Send {
                // The script never yields a connection, so nothing reaches here.
                std::future::pending()
            }

            fn handshake(
                (): (),
                _client: SocketAddr,
            ) -> impl Future<Output = Result<Box<dyn PendingPublish>, TransportError>> + Send
            {
                // The script never yields a connection, so nothing reaches here.
                std::future::pending()
            }
        }

        let recorder = Arc::new(Recorder::default());
        let node = node_with_events(
            NodeConfig::default(),
            Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>),
        )?;
        let (_stop, stop_rx) = watch::channel(false);
        let script = [
            Accepted::Failed("Too many open files".into()),
            Accepted::Failed("Too many open files".into()),
        ];
        let outcome = run_ingest(
            Scripted(script.into()),
            node.services().clone(),
            Arc::new(SessionConfig::default()),
            PendingPublishers::new(1),
            None,
            stop_rx,
        )
        .await;

        // Reaching the end of the script proves both failures were survived.
        assert!(matches!(outcome, Err(RuntimeError::SrtStopped)));
        let failures = recorder
            .0
            .lock()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    NodeEvent::ListenerAcceptFailed {
                        protocol: Protocol::Rtmp,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(failures, 2);
        Ok(())
    }

    mod rtmps {
        use std::io::IoSlice;

        use bytes::Bytes;
        use rtmpx::{
            Packet, Segments,
            handshake::{Handshake, HandshakeProgress, HandshakeRole},
            sessions::{
                ClientEvent, ClientOutput, ClientSession, ClientSessionConfig, PublishMode,
            },
        };
        use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
        use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

        use super::*;
        use crate::{domain::IngestProtocol, server::http::fixtures};

        type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

        /// A bound RTMPS listener serving a fresh self-signed certificate.
        async fn listener()
        -> Result<(RtmpsListener, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
            let directory = fixtures::scratch("rtmps");
            let (settings, certificate) = fixtures::write_pair(&directory, "origin.test");
            let (tls, watch) = http::rotating_ingest_server_config(
                TlsSettings {
                    min_version: rushls_tls::TlsVersion::Tls12,
                    ..settings
                },
                ProcessMeters::default(),
                Events::default(),
                Protocol::Rtmps,
            )?;
            Ok((
                RtmpsListener {
                    tcp: TcpListener::bind("127.0.0.1:0").await?,
                    acceptor: tokio_rustls::TlsAcceptor::from(tls),
                    config: RtmpConfig {
                        protocol: IngestProtocol::Rtmps,
                        ..RtmpConfig::default()
                    },
                    proxy_protocol: false,
                    _watch: watch,
                },
                certificate,
            ))
        }

        /// Runs one connection through every step the accept loop would.
        async fn negotiate(
            listener: &mut RtmpsListener,
        ) -> Result<Box<dyn PendingPublish>, TransportError> {
            let Accepted::Connection(connection) = listener.accept().await else {
                panic!("the listener accepts");
            };
            let (client, identified) = RtmpsListener::identify(connection).await?;
            RtmpsListener::handshake(identified, client).await
        }

        async fn write<S: AsyncWrite + Unpin, P: Segments>(
            io: &mut S,
            mut packet: Packet<P>,
        ) -> std::io::Result<()> {
            while !packet.is_complete() {
                let mut slices = [IoSlice::new(&[]); 32];
                let count = packet.io_slices(&mut slices);
                let written = io.write_vectored(&slices[..count]).await?;
                if written == 0 {
                    return Err(std::io::ErrorKind::WriteZero.into());
                }
                packet.advance(written);
            }
            io.flush().await
        }

        /// An encoder: RTMP handshake, `connect("live")`, `publish("camera")`,
        /// then it waits for the server to decide.
        async fn publish<S: AsyncRead + AsyncWrite + Unpin>(mut io: S) -> TestResult {
            let mut handshake = Handshake::new(HandshakeRole::Client);
            io.write_all(&handshake.generate_outbound_p0_and_p1()?)
                .await?;
            let mut buffer = vec![0; 16 * 1024];
            let mut input = loop {
                let read = io.read(&mut buffer).await?;
                if read == 0 {
                    return Err("closed during the RTMP handshake".into());
                }
                match handshake.process_bytes(&buffer[..read])? {
                    HandshakeProgress::InProgress { response_bytes } => {
                        io.write_all(&response_bytes).await?;
                    }
                    HandshakeProgress::Completed {
                        response_bytes,
                        remaining_bytes,
                    } => {
                        io.write_all(&response_bytes).await?;
                        break Bytes::from(remaining_bytes);
                    }
                }
            };
            let mut session = ClientSession::new(ClientSessionConfig::default())?;
            // Both queue their commands; `receive` hands them out as packets.
            session.connect("live")?;
            loop {
                while let Some(output) = session.receive(&mut input)? {
                    match output {
                        ClientOutput::Packet(packet) => write(&mut io, packet).await?,
                        ClientOutput::Event(ClientEvent::ConnectionRequestAccepted { .. }) => {
                            session.publish("camera", PublishMode::Live)?;
                        }
                        _ => {}
                    }
                }
                let read = io.read(&mut buffer).await?;
                if read == 0 {
                    return Ok(());
                }
                input = Bytes::copy_from_slice(&buffer[..read]);
            }
        }

        #[tokio::test]
        async fn a_tls_publisher_reaches_admission_as_rtmps() -> TestResult {
            let (mut listener, certificate) = listener().await?;
            let address = listener.tcp.local_addr()?;

            let mut roots = RootCertStore::empty();
            roots.add(certificate.into())?;
            let config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let encoder = tokio::spawn(async move {
                let tcp = tokio::net::TcpStream::connect(address).await?;
                let local = tcp.local_addr()?;
                let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                    .connect(ServerName::try_from("origin.test")?, tcp)
                    .await?;
                // The publish outcome is decided by the test, not the encoder.
                let _ = publish(tls).await;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(local)
            });

            let pending = negotiate(&mut listener).await?;
            let request = pending.publish_request()?;
            assert_eq!(request.protocol, IngestProtocol::Rtmps);
            assert_eq!(request.resource.namespace.as_deref(), Some("live"));
            assert_eq!(request.resource.name, "camera");
            drop(pending);
            let local = encoder.await??;
            assert_eq!(
                request.client.remote_address, local,
                "admission sees the encoder's own address"
            );
            Ok(())
        }

        #[tokio::test]
        async fn a_cleartext_rtmp_publisher_is_refused() -> TestResult {
            let (mut listener, _) = listener().await?;
            let address = listener.tcp.local_addr()?;
            let encoder = tokio::spawn(async move {
                let tcp = tokio::net::TcpStream::connect(address).await?;
                let _ = publish(tcp).await;
                Ok::<_, std::io::Error>(())
            });

            let Err(error) = negotiate(&mut listener).await else {
                panic!("RTMP without TLS must not reach admission");
            };
            assert!(error.to_string().contains("TLS handshake"), "{error}");
            encoder.await??;
            Ok(())
        }
    }

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

    /// PROXY headers name the client that `limits.publishers_per_address`
    /// counts, and a connection without one is refused rather than trusted.
    #[tokio::test]
    async fn proxied_clients_are_counted_per_address() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::AsyncWriteExt;

        let recorder = Arc::new(Recorder::default());
        let config = NodeConfig {
            rtmp_address: "127.0.0.1:0".parse()?,
            rtmp_proxy_protocol: true,
            srt_address: "127.0.0.1:0".parse()?,
            http_address: None,
            https_address: None,
            maximum_publishers_per_address: Some(nz::usize!(1)),
            ..NodeConfig::default()
        };
        let node = node_with_events(
            config,
            Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>),
        )?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(node.serve(async {
            let _ = stopped.await;
        }));
        let rtmp = recorder
            .find(|event| match event {
                NodeEvent::ListenerBound {
                    protocol: Protocol::Rtmp,
                    address,
                } => Some(*address),
                _ => None,
            })
            .await
            .ok_or("RTMP never bound")?;

        let proxied = |client: &'static str| async move {
            let mut stream = tokio::net::TcpStream::connect(rtmp).await?;
            stream
                .write_all(format!("PROXY TCP4 {client} 10.0.0.1 40000 1935\r\n").as_bytes())
                .await?;
            // Holds the connection mid-handshake, and so its permit.
            Ok::<_, std::io::Error>(stream)
        };
        let first = proxied("203.0.113.7").await?;
        let second = proxied("203.0.113.7").await?;
        let limited = recorder
            .find(|event| match event {
                NodeEvent::PublisherAddressLimited {
                    address, maximum, ..
                } => Some((address.clone(), *maximum)),
                _ => None,
            })
            .await;
        assert_eq!(limited, Some(("203.0.113.7".to_owned(), 1)));

        // A different client behind the same proxy is counted separately.
        let other = proxied("203.0.113.8").await?;
        // A peer that reaches the listener directly cannot skip the header.
        let mut direct = tokio::net::TcpStream::connect(rtmp).await?;
        direct.write_all(&[3; 16]).await?;
        let refused = recorder
            .find(|event| match event {
                NodeEvent::PublisherHandshakeFailed { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .await
            .ok_or("the headerless connection was not refused")?;
        assert!(refused.contains("PROXY protocol"), "{refused}");
        let limits = recorder
            .0
            .lock()
            .iter()
            .filter(|event| matches!(event, NodeEvent::PublisherAddressLimited { .. }))
            .count();
        assert_eq!(
            limits, 1,
            "only the second connection from one client is refused"
        );

        // Closed first, so the drain does not wait out their handshake deadline.
        drop((first, second, other, direct));
        let _ = stop.send(());
        serving.await??;
        Ok(())
    }

    #[tokio::test]
    async fn an_occupied_rtmp_port_fails_with_the_address_attached() -> Result<(), RuntimeError> {
        // A host-port collision must fail at startup: whatever the
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
