//! HTTPS/HTTP2 origin plus a publication, ready for Apple's playlist validator.

use std::{sync::Arc, time::Duration};

use rushls::{
    admission::{OpenStreamAuthenticator, StreamPolicy},
    delivery::hls::{service::PlaylistReadiness, uri::UriBase},
    observe::Events,
    segment::SegmentationPolicy,
    server::{
        Node, NodeConfig,
        http::{HttpConfig, Readiness, bind_tls, serve},
    },
    session::{PendingPermit, SessionConfig, SessionOutcome, run_session},
    source::PendingPublish,
};
use tokio::net::TcpListener;

use crate::{certs, validator};

pub struct Origin {
    node: Node,
    session: SessionConfig,
    url: String,
    ca_pem: std::path::PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    _https: Arc<certs::SharedHttps>,
}

impl Origin {
    pub async fn start() -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        if cfg!(not(target_os = "macos")) {
            eprintln!("skipping Apple HLS tests: macOS and mediastreamvalidator are required");
            return Ok(None);
        }
        if !validator::mediastreamvalidator_available() {
            eprintln!("skipping Apple HLS tests: mediastreamvalidator is not on PATH");
            return Ok(None);
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let Some(https) = certs::shared_https()? else {
            return Ok(None);
        };

        let mut config = NodeConfig::default();
        // Absolute HTTPS names so Apple fetches over TLS/HTTP2, not relative HTTP/1.1.
        config.hls.uri_base = UriBase::new(format!("https://127.0.0.1:{}", address.port()));
        config.hls.readiness = PlaylistReadiness::CompletedSegment;
        // Eight seconds of 1 s GOPs lock onto a 2 s / 0.5 s cadence with several
        // completed segments, without waiting on Apple's 6 s default.
        config.session.segmentation =
            SegmentationPolicy::latency_first(Duration::from_secs(2), Duration::from_millis(500));
        let session = config.session;
        let node = Node::new(
            config,
            Arc::new(OpenStreamAuthenticator::new(StreamPolicy::permissive())),
            Events::default(),
        )?;

        let settings = https.settings.clone();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let tls_listener = bind_tls(
            listener,
            settings.clone(),
            node.services().meters.clone(),
            node.services().events.clone(),
        )?;
        let server = tokio::spawn(serve(
            tls_listener,
            node.application(),
            HttpConfig {
                tls: Some(settings),
                tls_address: Some(address),
                ..HttpConfig::default()
            },
            None,
            Readiness::ready(),
            async {
                let _ = stopped.await;
            },
        ));

        Ok(Some(Self {
            node,
            session,
            url: format!(
                "https://127.0.0.1:{}/live/camera/index.m3u8",
                address.port()
            ),
            ca_pem: https.ca_pem.clone(),
            shutdown: Some(shutdown),
            server: Some(server),
            _https: https,
        }))
    }

    pub async fn publish_and_validate(
        &self,
        pending: Box<dyn PendingPublish>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let outcome = run_session(
            pending,
            self.node.services(),
            &self.session,
            PendingPermit::unlimited(),
        )
        .await;
        match outcome {
            Ok(SessionOutcome::Ended | SessionOutcome::Interrupted) => {}
            Ok(other) => return Err(format!("session ended unexpectedly: {other:?}").into()),
            Err(error) => return Err(error.into()),
        }
        self.validate_playlist()
    }

    /// Runs the session in the background so a live transport can stay open
    /// while Apple fetches. The caller closes the publisher after this returns.
    pub fn spawn_session(
        &self,
        pending: Box<dyn PendingPublish>,
    ) -> tokio::task::JoinHandle<Result<SessionOutcome, rushls::session::SessionError>> {
        let services = self.node.services().clone();
        let session = self.session;
        tokio::spawn(async move {
            run_session(pending, &services, &session, PendingPermit::unlimited()).await
        })
    }

    pub fn validate_playlist(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        validator::wait_for_playlist(&self.url, &self.ca_pem)?;
        validator::validate(&self.url)?;
        Ok(())
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}
