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
        serve::{DeliveryConfig, Origin},
    },
    media::PassThroughNormalizerFactory,
    mux::{CmafMuxerConfig, PassThroughMuxerFactory},
    observe::{Events, ProcessMeters},
    session::{Registry, Services, SessionConfig, StopReason, run_session},
    source::transport::rtmp::{RtmpConfig, RtmpPendingPublish},
};

use super::{
    http::{self, HttpConfig},
    metrics::{ExportPolicy, MetricsReader},
};

/// Process-level configuration for one self-contained origin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeConfig {
    pub rtmp_address: SocketAddr,
    pub http_address: SocketAddr,
    pub maintenance_interval: Duration,
    pub maximum_sessions: usize,
    pub rtmp: RtmpConfig,
    pub session: SessionConfig,
    pub cmaf: CmafMuxerConfig,
    pub store: StoreLimits,
    pub delivery: DeliveryConfig,
    pub http: HttpConfig,
    pub metrics: ExportPolicy,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            rtmp_address: "0.0.0.0:1935".parse().expect("constant address is valid"),
            http_address: "0.0.0.0:8080".parse().expect("constant address is valid"),
            maintenance_interval: Duration::from_secs(1),
            maximum_sessions: 256,
            rtmp: RtmpConfig::default(),
            session: SessionConfig::default(),
            cmaf: CmafMuxerConfig::default(),
            store: StoreLimits::default(),
            delivery: DeliveryConfig::default(),
            http: HttpConfig::default(),
            metrics: ExportPolicy::default(),
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
    #[error("could not bind HTTP at {address}: {source}")]
    BindHttp {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("RTMP listener failed: {0}")]
    Rtmp(std::io::Error),
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
    metrics: MetricsReader,
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
        if config.maintenance_interval.is_zero() {
            return Err(RuntimeError::InvalidConfiguration(
                "maintenance interval must be nonzero",
            ));
        }

        // There is one input policy for a publication. Keeping the RTMP
        // adapter and the session driver on the same value prevents bytes that
        // passed one boundary from being refused by the next under a different
        // supposedly process-wide limit.
        config.rtmp.input_limits = config.session.input;
        let store = StreamStore::new(config.store);
        let sessions = Registry::with_capacity(config.maximum_sessions);
        let meters = ProcessMeters::default();
        let services = Services {
            authenticator,
            normalizers: Arc::new(PassThroughNormalizerFactory),
            muxers: Arc::new(PassThroughMuxerFactory::new(config.cmaf)),
            publishers: Arc::new(StorePublisherFactory::new(store.clone())),
            sessions: sessions.clone(),
            meters: meters.clone(),
            events,
        };
        let origin = Arc::new(Origin::new(store.clone(), config.delivery));
        let metrics = MetricsReader::new(meters, sessions, store.clone(), config.metrics);

        Ok(Self {
            config,
            services,
            store,
            origin,
            metrics,
        })
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
        let http_listener =
            TcpListener::bind(self.config.http_address)
                .await
                .map_err(|source| RuntimeError::BindHttp {
                    address: self.config.http_address,
                    source,
                })?;

        let (stop_tx, stop_rx) = watch::channel(false);
        let mut tasks = JoinSet::new();
        tasks.spawn(run_rtmp(
            rtmp_listener,
            self.config.rtmp,
            self.services.clone(),
            self.config.session,
            stop_rx.clone(),
        ));
        tasks.spawn(run_http(
            http_listener,
            Arc::clone(&self.origin),
            self.config.http,
            stop_rx.clone(),
        ));
        tasks.spawn(run_maintenance(
            self.store.clone(),
            Arc::clone(&self.origin),
            self.config.maintenance_interval,
            stop_rx,
        ));

        tokio::pin!(shutdown);
        let mut first_error = tokio::select! {
            _ = &mut shutdown => None,
            joined = tasks.join_next() => joined.and_then(task_error),
        };

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
}

async fn run_rtmp(
    listener: TcpListener,
    config: RtmpConfig,
    services: Services,
    session_config: SessionConfig,
    mut stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;

            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }

            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    eprintln!("RTMP connection task failed: {error}");
                }
            }

            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(RuntimeError::Rtmp)?;
                let services = services.clone();
                connections.spawn(async move {
                    let pending = match RtmpPendingPublish::handshake_tcp(stream, config).await {
                        Ok(pending) => pending,
                        Err(error) => {
                            eprintln!("RTMP handshake rejected: {error}");
                            return;
                        }
                    };
                    if let Err(error) =
                        run_session(Box::new(pending), &services, &session_config).await
                    {
                        eprintln!("publishing session failed: {error}");
                    }
                });
            }
        }
    }

    while let Some(completed) = connections.join_next().await {
        if let Err(error) = completed {
            eprintln!("RTMP connection task failed during shutdown: {error}");
        }
    }
    Ok(())
}

async fn run_http(
    listener: TcpListener,
    origin: Arc<Origin>,
    config: HttpConfig,
    stop: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    http::serve(listener, origin, config, wait_for_stop(stop))
        .await
        .map_err(RuntimeError::Http)
}

async fn run_maintenance(
    store: StreamStore,
    origin: Arc<Origin>,
    interval: Duration,
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
                store.maintain();
                origin.prune();
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
        admission::{FixedStreamAuthenticator, Principal, PublishGrant, StreamPolicy},
        domain::StreamId,
    };

    use super::*;

    fn node(config: NodeConfig) -> Result<Node, RuntimeError> {
        Node::new(
            config,
            Arc::new(FixedStreamAuthenticator::new(
                "secret",
                PublishGrant {
                    stream_id: StreamId::new("live/camera"),
                    principal: Principal("publisher".into()),
                    policy: StreamPolicy::permissive(),
                },
            )),
            Events::default(),
        )
    }

    #[test]
    fn one_input_limit_is_shared_by_transport_and_session() {
        let mut config = NodeConfig::default();
        config.session.input.maximum_packets_per_batch = 17;
        config.rtmp.input_limits.maximum_packets_per_batch = 99;

        let node = node(config).expect("configuration is valid");

        assert_eq!(node.config.rtmp.input_limits.maximum_packets_per_batch, 17);
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

    #[tokio::test]
    async fn an_immediate_shutdown_stops_every_runtime_task() {
        let node = node(NodeConfig {
            rtmp_address: "127.0.0.1:0".parse().expect("constant is valid"),
            http_address: "127.0.0.1:0".parse().expect("constant is valid"),
            ..NodeConfig::default()
        })
        .expect("configuration is valid");

        node.serve(async {}).await.expect("node stops cleanly");
    }
}
