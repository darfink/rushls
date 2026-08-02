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
        uri::{MediaResourcePath, parse_media_path},
    },
    hooks::Hooks,
    media::PassThroughNormalizerFactory,
    mux::{CmafMuxerConfig, PassThroughMuxerFactory},
    observe::{Events, NodeEvent, ProcessMeters, Protocol, StreamEvent},
    session::{PendingPublishers, Registry, Services, SessionConfig, StopReason, run_session},
    source::transport::{
        rtmp::{RtmpConfig, RtmpPendingPublish},
        srt::{SrtConfig, SrtListener},
    },
};

use super::{
    http::{self, Application, HttpConfig, Readiness, TlsError},
    metrics::{MetricsConfig, MetricsEndpoint, MetricsReader},
};

/// Process-level configuration for one self-contained origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeConfig {
    pub rtmp_address: SocketAddr,
    pub srt_address: SocketAddr,
    pub http_address: SocketAddr,
    pub maintenance_interval: Duration,
    pub maximum_sessions: usize,
    /// Connections one ingest listener may be admitting at once.
    ///
    /// Counted per listener rather than process-wide so that a flood on one
    /// transport cannot deny admission on the other; the process ceiling is
    /// this value times the number of ingest listeners. Separate from
    /// [`Self::maximum_sessions`], which bounds publishers that already
    /// authenticated: a node at its session capacity keeps its full admission
    /// headroom, and still rejects the surplus in the source protocol.
    ///
    /// A permit is held across authentication, so this is also what bounds
    /// concurrent requests to an external authentication provider.
    pub maximum_pending_publishers_per_listener: usize,
    pub rtmp: RtmpConfig,
    pub srt: SrtConfig,
    pub session: SessionConfig,
    pub cmaf: CmafMuxerConfig,
    pub store: StoreLimits,
    pub hls: HlsConfig,
    pub http: HttpConfig,
    pub metrics: MetricsConfig,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            rtmp_address: "0.0.0.0:1935".parse().expect("constant address is valid"),
            srt_address: "[::]:9000".parse().expect("constant address is valid"),
            http_address: "0.0.0.0:8080".parse().expect("constant address is valid"),
            maintenance_interval: Duration::from_secs(1),
            maximum_sessions: 256,
            maximum_pending_publishers_per_listener: 64,
            rtmp: RtmpConfig::default(),
            srt: SrtConfig::default(),
            session: SessionConfig::default(),
            cmaf: CmafMuxerConfig::default(),
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
    #[error("RTMP listener failed: {0}")]
    Rtmp(std::io::Error),
    #[error("SRT listener stopped unexpectedly")]
    SrtStopped,
    #[error("HTTP server failed: {0}")]
    Http(std::io::Error),
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
}

/// The protocols sharing one viewer-facing HTTP namespace.
///
/// Runtime is the composition root and therefore the only layer that names
/// both the HTTP port and a manifest adapter. Shared media bypasses adapters;
/// only manifest paths are delegated to HLS.
pub(crate) struct ViewerApplication {
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
            .map(|rendition| Duration::from_secs(rendition.contract.target_duration.get()));
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
            let target = Duration::from_secs(object.contract.target_duration.get());
            DeliveryResponse::media(
                object.body,
                object.gzip,
                named.resource,
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
    ) -> Result<Self, RuntimeError> {
        if config.maximum_sessions == 0 {
            return Err(RuntimeError::InvalidConfiguration(
                "maximum sessions must be nonzero",
            ));
        }
        if config.maximum_pending_publishers_per_listener == 0 {
            return Err(RuntimeError::InvalidConfiguration(
                "maximum pending publishers must be nonzero",
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
            .is_some_and(|token| token.is_empty())
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

        // There is one input policy for a publication. Keeping both transport
        // adapters and the session driver on the same value prevents bytes
        // that passed one boundary from being refused by the next under a
        // different supposedly process-wide limit.
        config.rtmp.input_limits = config.session.input;
        config.srt.input_limits = config.session.input;
        let store = StreamStore::new(config.store);
        let sessions = Registry::with_capacity(config.maximum_sessions);
        let meters = ProcessMeters::default();
        let services = Services {
            authenticator,
            normalizers: Arc::new(PassThroughNormalizerFactory),
            muxers: Arc::new(PassThroughMuxerFactory::new(config.cmaf)),
            publishers: Arc::new(
                StorePublisherFactory::new(store.clone()).with_events(events.clone()),
            ),
            sessions: sessions.clone(),
            meters: meters.clone(),
            events,
        };
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
            config.metrics.export,
        );

        Ok(Self {
            config,
            services,
            store,
            origin,
            hls,
            application,
            metrics,
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

    #[cfg(test)]
    pub(crate) fn application(&self) -> Arc<ViewerApplication> {
        Arc::clone(&self.application)
    }

    pub fn metrics(&self) -> &MetricsReader {
        &self.metrics
    }

    /// Runs both listeners and maintenance until `shutdown` resolves.
    pub async fn serve(
        self,
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
        .map_err(|source| RuntimeError::BindSrt {
            address: self.config.srt_address,
            source,
        })?;
        let http_listener =
            TcpListener::bind(self.config.http_address)
                .await
                .map_err(|source| RuntimeError::BindHttp {
                    address: self.config.http_address,
                    source,
                })?;

        let events = self.services.events.clone();
        // Reported from the bound listeners rather than from configuration,
        // because with an ephemeral port the configured value is a zero and
        // the real one exists nowhere else.
        report_bound(&events, Protocol::Rtmp, rtmp_listener.local_addr());
        report_bound(&events, Protocol::Srt, Ok(srt_listener.local_address()));

        let (stop_tx, stop_rx) = watch::channel(false);
        let readiness = Readiness::default();
        let mut tasks = JoinSet::new();
        // One budget each: a transport being flooded with connections that
        // never authenticate should not stop the other from admitting anyone.
        tasks.spawn(run_rtmp(
            rtmp_listener,
            self.config.rtmp,
            self.services.clone(),
            self.config.session,
            PendingPublishers::new(self.config.maximum_pending_publishers_per_listener),
            stop_rx.clone(),
        ));
        tasks.spawn(run_srt(
            srt_listener,
            self.services.clone(),
            self.config.session,
            PendingPublishers::new(self.config.maximum_pending_publishers_per_listener),
            stop_rx.clone(),
        ));
        // The listener type differs but the server does not: both arms run the
        // same router, the same graceful shutdown, and the same task.
        //
        // HTTPS is announced only once TLS is actually up. Announcing it
        // alongside the other listeners would put "HTTPS listening on …" in
        // the log immediately above the certificate error that stopped the
        // process from ever serving.
        match self.config.http.tls.clone() {
            Some(settings) => {
                let listener = http::bind_tls(
                    http_listener,
                    settings,
                    self.services.meters.clone(),
                    events.clone(),
                )?;
                report_bound(&events, Protocol::Https, listener.local_addr());
                tasks.spawn(run_http(
                    listener,
                    Arc::clone(&self.application),
                    self.config.http.clone(),
                    self.metrics_endpoint(),
                    readiness.clone(),
                    stop_rx.clone(),
                ));
            }
            None => {
                report_bound(&events, Protocol::Http, http_listener.local_addr());
                tasks.spawn(run_http(
                    http_listener,
                    Arc::clone(&self.application),
                    self.config.http.clone(),
                    self.metrics_endpoint(),
                    readiness.clone(),
                    stop_rx.clone(),
                ));
            }
        }
        tasks.spawn(run_maintenance(
            self.store.clone(),
            Arc::clone(&self.hls),
            self.config.maintenance_interval,
            events.clone(),
            stop_rx,
        ));
        // All listeners and long-running tasks now exist. During shutdown this
        // flips before their drain begins, so load balancers stop adding work.
        readiness.mark_ready();

        tokio::pin!(shutdown);
        let (shutdown_requested, mut first_error) = tokio::select! {
            _ = &mut shutdown => (true, None),
            joined = tasks.join_next() => (false, joined.and_then(task_error)),
        };

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

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn metrics_endpoint(&self) -> Option<MetricsEndpoint> {
        self.config
            .metrics
            .enabled
            .then(|| MetricsEndpoint::new(self.metrics.clone(), self.config.metrics.token.clone()))
    }
}

async fn run_srt(
    mut listener: SrtListener,
    services: Services,
    session_config: SessionConfig,
    pending_publishers: PendingPublishers,
    mut stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut connections = JoinSet::new();
    loop {
        // Reserved before accepting, for the reason given in `run_rtmp`. SRT's
        // own listener backlog bounds what waits ahead of `accept`; nothing
        // bounded what came after it.
        let slot = tokio::select! {
            biased;

            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
                continue;
            }

            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    services.events.emit(NodeEvent::ConnectionTaskPanicked {
                        protocol: Protocol::Srt,
                        reason: error.to_string(),
                    });
                }
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
                let Some(accepted) = accepted else {
                    return Err(RuntimeError::SrtStopped);
                };
                let pending = match accepted {
                    Ok(pending) => pending,
                    Err(error) => {
                        services.events.emit(NodeEvent::PublisherHandshakeFailed {
                            protocol: Protocol::Srt,
                            reason: error.to_string(),
                        });
                        continue;
                    }
                };
                let services = services.clone();
                connections.spawn(async move {
                    if let Err(error) =
                        run_session(Box::new(pending), &services, &session_config, slot).await
                    {
                        services.events.emit(NodeEvent::PublisherSessionFailed {
                            protocol: Protocol::Srt,
                            reason: error.to_string(),
                        });
                    }
                });
            }
        }
    }

    drop(listener);
    while let Some(completed) = connections.join_next().await {
        if let Err(error) = completed {
            services.events.emit(NodeEvent::ConnectionTaskPanicked {
                protocol: Protocol::Srt,
                reason: error.to_string(),
            });
        }
    }
    Ok(())
}

async fn run_rtmp(
    listener: TcpListener,
    config: RtmpConfig,
    services: Services,
    session_config: SessionConfig,
    pending_publishers: PendingPublishers,
    mut stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut connections = JoinSet::new();
    loop {
        // Capacity is reserved *before* accepting, so a surplus of connections
        // waits in the kernel's backlog instead of becoming tasks that nothing
        // has authenticated. Accepting and then closing would give an attacker
        // cheap connection churn and give a well-behaved encoder nothing to
        // retry against.
        let slot = tokio::select! {
            biased;

            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
                continue;
            }

            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    services.events.emit(NodeEvent::ConnectionTaskPanicked {
                        protocol: Protocol::Rtmp,
                        reason: error.to_string(),
                    });
                }
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
                let (stream, _) = accepted.map_err(RuntimeError::Rtmp)?;
                let services = services.clone();
                connections.spawn(async move {
                    let pending = match RtmpPendingPublish::handshake_tcp(stream, config).await {
                        Ok(pending) => pending,
                        Err(error) => {
                            services.events.emit(NodeEvent::PublisherHandshakeFailed {
                            protocol: Protocol::Rtmp,
                            reason: error.to_string(),
                        });
                            return;
                        }
                    };
                    if let Err(error) =
                        run_session(Box::new(pending), &services, &session_config, slot).await
                    {
                        services.events.emit(NodeEvent::PublisherSessionFailed {
                            protocol: Protocol::Rtmp,
                            reason: error.to_string(),
                        });
                    }
                });
            }
        }
    }

    while let Some(completed) = connections.join_next().await {
        if let Err(error) = completed {
            services.events.emit(NodeEvent::ConnectionTaskPanicked {
                protocol: Protocol::Rtmp,
                reason: error.to_string(),
            });
        }
    }
    Ok(())
}

async fn run_http<L>(
    listener: L,
    application: Arc<ViewerApplication>,
    config: HttpConfig,
    metrics: Option<MetricsEndpoint>,
    readiness: Readiness,
    stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    http::serve_with_readiness(
        listener,
        application,
        config,
        metrics,
        readiness,
        wait_for_stop(stop),
    )
    .await
    .map_err(RuntimeError::Http)
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
        admission::{
            Principal, PublishGrant, StaticPublisher, StaticStreamAuthenticator, StreamPolicy,
        },
        domain::StreamId,
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
            Arc::new(StaticStreamAuthenticator::new(vec![StaticPublisher::new(
                "secret",
                PublishGrant {
                    stream_id: StreamId::new("live/camera"),
                    principal: Principal("publisher".into()),
                    policy: StreamPolicy::permissive(),
                },
            )])),
            events,
        )
    }

    #[test]
    fn one_input_limit_is_shared_by_transport_and_session() {
        let mut config = NodeConfig::default();
        config.session.input.maximum_packets_per_batch = 17;
        config.rtmp.input_limits.maximum_packets_per_batch = 99;
        config.srt.input_limits.maximum_packets_per_batch = 98;

        let node = node(config).expect("configuration is valid");

        assert_eq!(node.config.rtmp.input_limits.maximum_packets_per_batch, 17);
        assert_eq!(node.config.srt.input_limits.maximum_packets_per_batch, 17);
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
        assert!(matches!(
            node(NodeConfig {
                maximum_pending_publishers_per_listener: 0,
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

        let recorder = Arc::new(Recorder::default());
        let node = node_with_events(
            NodeConfig {
                rtmp_address: "127.0.0.1:0".parse().expect("constant is valid"),
                srt_address: "127.0.0.1:0".parse().expect("constant is valid"),
                http_address: "127.0.0.1:0".parse().expect("constant is valid"),
                ..NodeConfig::default()
            },
            Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>),
        )?;

        node.serve(async {}).await?;
        assert!(matches!(
            recorder.0.lock().last(),
            Some(NodeEvent::ShuttingDown)
        ));
        Ok(())
    }
}
