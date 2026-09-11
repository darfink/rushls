//! WebTransport and raw QUIC moq-lite ingest listener.
//!
//! Handshake (TLS, CONNECT, SETUP, first announcement) runs on the connection task, the
//! same way RTMP does: a peer that stalls mid-SETUP must not hold up accept.
//! One MOQ session is one publication. A second broadcast on the same
//! connection is refused once discovery has frozen the first.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use cc_tls::CertificateWatch;
use rustls::ServerConfig;
use tokio::task::JoinHandle;
use web_transport_quinn::http::StatusCode;

use crate::{
    admission::{ClientInfo, IngestProtocol, PublishGrant, PublishRequest},
    domain::BoxFuture,
    observe::SourceMeters,
    source::{
        AcceptedPublish, InputLimits, PendingPublish, PublishRejection, TransportError,
        moq::{MoqPacketSource, from_webtransport_url},
    },
};

/// Pinned so a browser and this origin never silently negotiate an IETF draft.
const MOQ_LITE_05: &str = "moq-lite-05";

/// Resource, idle, and TLS policy for one MOQ listener.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoqConfig {
    /// Idle timeout for an established WebTransport session. `None` disables it.
    pub idle_timeout: Option<Duration>,
    /// Unauthenticated SETUP/CONNECT budget, derived from [`Self::idle_timeout`].
    pub handshake_timeout: Duration,
    pub tls: Option<cc_tls::TlsSettings>,
    pub input_limits: InputLimits,
}

impl Default for MoqConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Some(Duration::from_secs(10)),
            handshake_timeout: Duration::from_secs(5),
            tls: None,
            input_limits: InputLimits::permissive(),
        }
    }
}

/// The same port on the other loopback family, when that is worth binding.
///
/// `None` for any address that is not loopback, because widening a public
/// listener to a second family is an operator's decision rather than a
/// convenience, and `None` for port zero, where the sibling would be handed
/// an unrelated ephemeral port and silently listen somewhere nobody dialled.
fn loopback_companion(address: SocketAddr) -> Option<SocketAddr> {
    if address.port() == 0 || !address.ip().is_loopback() {
        return None;
    }
    let sibling = match address.ip() {
        IpAddr::V4(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    Some(SocketAddr::new(sibling, address.port()))
}

pub struct MoqListener {
    endpoint: web_transport_quinn::quinn::Endpoint,
    /// The other loopback family, when the primary address is loopback.
    ///
    /// A browser resolving `localhost` picks one family and, unlike TCP, QUIC
    /// has no happy-eyeballs fallback to the other: a datagram sent to the
    /// address nothing is bound to is simply refused, and the page reports an
    /// opaque handshake failure. Binding both loopback addresses makes the
    /// resolver's choice stop mattering without widening the listener beyond
    /// loopback, which a dual-stack `[::]` socket would do.
    companion: Option<web_transport_quinn::quinn::Endpoint>,
    config: MoqConfig,
    _watch: CertificateWatch,
}

impl MoqListener {
    pub fn bind(
        address: SocketAddr,
        tls: Arc<ServerConfig>,
        watch: CertificateWatch,
        config: MoqConfig,
    ) -> Result<Self, TransportError> {
        let mut tls = Arc::unwrap_or_clone(tls);
        tls.alpn_protocols = vec![b"h3".to_vec(), MOQ_LITE_05.as_bytes().to_vec()];
        let quic = web_transport_quinn::quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|error| {
                TransportError::Handshake(
                    format!("QUIC TLS could not be configured: {error}").into(),
                )
            })?;
        let mut server_config =
            web_transport_quinn::quinn::ServerConfig::with_crypto(Arc::new(quic));
        let mut transport = web_transport_quinn::quinn::TransportConfig::default();
        match config.idle_timeout {
            Some(idle) => {
                let timeout =
                    web_transport_quinn::quinn::IdleTimeout::try_from(idle).map_err(|_| {
                        TransportError::Handshake("the MOQ idle timeout is too large".into())
                    })?;
                transport.max_idle_timeout(Some(timeout));
            }
            None => {
                transport.max_idle_timeout(None);
            }
        }
        server_config.transport_config(Arc::new(transport));
        let endpoint = web_transport_quinn::quinn::Endpoint::server(server_config.clone(), address)
            .map_err(|error| TransportError::Handshake(error.to_string().into()))?;
        // Failing to bind the sibling is a warning rather than an error: a
        // host with one loopback family disabled still serves the family it
        // has, and refusing to start would be a worse answer than a listener
        // that a browser might not find.
        let companion = loopback_companion(address).and_then(|sibling| {
            match web_transport_quinn::quinn::Endpoint::server(server_config, sibling) {
                Ok(endpoint) => {
                    tracing::info!(protocol = "moq", address = %sibling, "listening on the sibling loopback address");
                    Some(endpoint)
                }
                Err(error) => {
                    tracing::warn!(
                        address = %sibling,
                        %error,
                        "the sibling loopback address could not be bound, so a browser \
                         resolving localhost to it will not reach this listener"
                    );
                    None
                }
            }
        });
        Ok(Self {
            endpoint,
            companion,
            config,
            _watch: watch,
        })
    }

    pub fn local_address(&self) -> Result<SocketAddr, TransportError> {
        self.endpoint
            .local_addr()
            .map_err(|error| TransportError::Handshake(error.to_string().into()))
    }

    pub async fn accept(&mut self) -> Option<web_transport_quinn::quinn::Incoming> {
        match &self.companion {
            // Both arms are cancel-safe: a dropped `Accept` leaves the
            // endpoint's queue untouched, so the loser of one race is still
            // offered on the next call.
            Some(companion) => tokio::select! {
                incoming = self.endpoint.accept() => incoming,
                incoming = companion.accept() => incoming,
            },
            None => self.endpoint.accept().await,
        }
    }

    pub fn config(&self) -> MoqConfig {
        self.config.clone()
    }
}

pub struct MoqPendingPublish {
    request: PublishRequest,
    origin: moq_net::origin::Producer,
    session: moq_net::Session,
    driver: JoinHandle<Result<(), moq_net::Error>>,
    config: MoqConfig,
}

impl MoqPendingPublish {
    pub async fn handshake(
        request: web_transport_quinn::quinn::Incoming,
        config: MoqConfig,
    ) -> Result<Self, TransportError> {
        tokio::time::timeout(config.handshake_timeout, handshake_inner(request, config))
            .await
            .map_err(|_| {
                TransportError::Handshake(
                    "MOQ publisher did not finish SETUP before the deadline".into(),
                )
            })?
    }
}

async fn handshake_inner(
    wt_request: web_transport_quinn::quinn::Incoming,
    config: MoqConfig,
) -> Result<MoqPendingPublish, TransportError> {
    let connection = wt_request.await.map_err(|error| {
        TransportError::Handshake(format!("QUIC handshake failed: {error}").into())
    })?;
    let remote_address = connection.remote_address();
    let (session, partial) = accept_transport(connection).await?;

    let version =
        moq_net::Version::from_str(MOQ_LITE_05).expect("moq-lite-05 is a supported version string");
    let moq_request = moq_net::Server::new()
        .with_versions(version.into())
        .accept_request(session)
        .await
        .map_err(|error| {
            TransportError::Handshake(format!("moq-lite SETUP failed: {error}").into())
        })?;

    let setup_path = moq_request.path().to_owned();
    // Bound the model's cache target as well as the batches we retain. The
    // dependency treats this as an eviction target, not a hard allocation cap.
    let origin = moq_net::origin::Info::new(moq_net::Origin::random())
        .with_pool(moq_net::cache::Pool::new(
            crate::source::PipelineMemory::TRANSPORT as u64,
        ))
        .with_cache_duration(Duration::from_secs(30))
        .produce();
    let (session, driver) = moq_request
        .with_subscriber(origin.clone())
        .ok()
        .await
        .map_err(|error| {
            TransportError::Handshake(format!("moq-lite session failed: {error}").into())
        })?;
    let driver = tokio::spawn(driver);
    // Root connections name their publication in ANNOUNCE, not SETUP. The
    // source later gets a new cursor, which replays the origin's live announcements.
    let mut announced = origin.consume().announced();
    let identity_path = if partial.resource.is_none() && setup_path.trim_matches('/').is_empty() {
        loop {
            let announce = announced.next().await.ok_or_else(|| {
                TransportError::Handshake(
                    "MOQ publisher closed before announcing a broadcast".into(),
                )
            })?;
            if announce.broadcast.is_some() {
                break announce.path.to_string();
            }
        }
    } else {
        setup_path
    };
    let identity = match partial.complete_with_setup(&identity_path) {
        Ok(identity) => identity,
        Err(error) => {
            session.abort(moq_net::Error::Unauthorized);
            return Err(error);
        }
    };

    Ok(MoqPendingPublish {
        request: PublishRequest {
            protocol: IngestProtocol::Moq,
            resource: identity.resource,
            credential: identity.credential,
            client: ClientInfo {
                remote_address,
                encoder: None,
                protocol_version: Some(MOQ_LITE_05.to_owned()),
            },
        },
        origin,
        session,
        driver,
        config,
    })
}

/// Select the same pinned protocol through the transport's negotiation surface.
async fn accept_transport(
    connection: web_transport_quinn::quinn::Connection,
) -> Result<
    (
        web_transport_quinn::Session,
        crate::source::moq::identity::PartialIdentity,
    ),
    TransportError,
> {
    let raw = web_transport_quinn::Session::raw(connection.clone());
    let result = if raw.protocol() == Some(MOQ_LITE_05) {
        (
            raw,
            crate::source::moq::identity::PartialIdentity {
                resource: None,
                credential: None,
            },
        )
    } else {
        let request = web_transport_quinn::Request::accept(connection)
            .await
            .map_err(|error| {
                TransportError::Handshake(format!("WebTransport CONNECT failed: {error}").into())
            })?;
        if !request
            .protocols
            .iter()
            .any(|protocol| protocol == MOQ_LITE_05)
        {
            let _ = request.reject(StatusCode::BAD_REQUEST).await;
            return Err(TransportError::Handshake(
                "WebTransport requires moq-lite-05".into(),
            ));
        }
        let partial = match from_webtransport_url(&request.url) {
            Ok(partial) => partial,
            Err(error) => {
                let _ = request.reject(StatusCode::BAD_REQUEST).await;
                return Err(error);
            }
        };
        let session = request
            .respond(web_transport_quinn::proto::ConnectResponse::OK.with_protocol(MOQ_LITE_05))
            .await
            .map_err(|error| {
                TransportError::Handshake(format!("WebTransport CONNECT failed: {error}").into())
            })?;
        (session, partial)
    };
    Ok(result)
}

impl PendingPublish for MoqPendingPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(self.request.clone())
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let source = MoqPacketSource::from_origin(
                &self.origin.consume(),
                Some(self.session),
                self.config.input_limits,
                meters,
            )
            .map_err(|error| TransportError::Accept(error.to_string().into()))?;
            Ok(AcceptedPublish {
                source: Box::new(source),
                grant,
            })
        })
    }

    fn reject(
        self: Box<Self>,
        rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async move {
            self.session.abort(abort_reason(rejection));
            self.driver.abort();
            Ok(())
        })
    }
}

fn abort_reason(rejection: PublishRejection) -> moq_net::Error {
    match rejection {
        PublishRejection::Unauthorized | PublishRejection::Forbidden => {
            moq_net::Error::Unauthorized
        }
        PublishRejection::AlreadyPublished => moq_net::Error::Duplicate,
        PublishRejection::ServiceUnavailable => moq_net::Error::Timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::{Events, ProcessMeters, Protocol};
    use web_transport_quinn::{proto::ConnectRequest, quinn};

    async fn negotiate(raw: bool) -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::server::http::fixtures::scratch("moq-handshake");
        let (settings, certificate) =
            crate::server::http::fixtures::write_pair(&directory, "127.0.0.1");
        let (tls, watch) = crate::server::http::rotating_quic_server_config(
            settings,
            ProcessMeters::default(),
            Events::default(),
            Protocol::Moq,
        )?;
        let mut listener =
            MoqListener::bind("127.0.0.1:0".parse()?, tls, watch, MoqConfig::default())?;
        let address = listener.local_address()?;
        let client = async {
            let origin = moq_net::Origin::random().produce();
            let broadcast = origin.create_broadcast(
                "live/camera",
                moq_net::broadcast::Route::new().with_announce(true),
            )?;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(rustls::pki_types::CertificateDer::from(certificate))?;
            let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
            tls.alpn_protocols = vec![if raw {
                MOQ_LITE_05.as_bytes().to_vec()
            } else {
                b"h3".to_vec()
            }];
            let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
            let config = quinn::ClientConfig::new(Arc::new(crypto));
            let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
            let transport = if raw {
                let connection = endpoint.connect_with(config, address, "127.0.0.1")?.await?;
                web_transport_quinn::Session::raw(connection)
            } else {
                let client = web_transport_quinn::Client::new(endpoint, config);
                client
                    .connect(
                        ConnectRequest::new(url::Url::parse(&format!(
                            "https://127.0.0.1:{}/",
                            address.port()
                        ))?)
                        .with_protocol(MOQ_LITE_05),
                    )
                    .await?
            };
            assert_eq!(transport.protocol(), Some(MOQ_LITE_05));
            let version = moq_net::Version::from_str(MOQ_LITE_05)?;
            let (session, driver) = moq_net::Client::new()
                .with_versions(version.into())
                .with_publisher(&origin)
                .connect(transport)
                .await?;
            let driver = tokio::spawn(driver);
            Ok::<_, Box<dyn std::error::Error>>((session, driver, origin, broadcast))
        };
        let server = async {
            let incoming = listener.accept().await.ok_or("listener closed")?;
            Ok::<_, Box<dyn std::error::Error>>(
                MoqPendingPublish::handshake(incoming, listener.config()).await?,
            )
        };
        let (client, pending) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(Box::pin(client), Box::pin(server))
        })
        .await?;
        let (session, driver, _origin, mut broadcast) = client?;
        let pending = pending?;
        let request = pending.publish_request()?;
        assert_eq!(request.resource.namespace.as_deref(), Some("live"));
        assert_eq!(request.resource.name, "camera");
        assert_eq!(request.credential.expose(), b"camera");
        Box::new(pending)
            .reject(PublishRejection::Forbidden)
            .await?;
        broadcast.finish();
        session.abort(moq_net::Error::Cancel);
        driver.abort();
        drop(listener);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn webtransport_selects_lite05_and_root_uses_announcement()
    -> Result<(), Box<dyn std::error::Error>> {
        negotiate(false).await
    }

    #[tokio::test]
    async fn raw_quic_negotiates_lite05() -> Result<(), Box<dyn std::error::Error>> {
        negotiate(true).await
    }
}
