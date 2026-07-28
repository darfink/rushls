use std::{env, error::Error, net::SocketAddr, sync::Arc};

use rushls::{
    admission::{FixedStreamAuthenticator, Principal, PublishGrant, StreamPolicy},
    delivery::hls::uri::UriBase,
    domain::{SessionId, StreamId},
    observe::{EventObserver, Events, SessionEvent},
    server::{Node, NodeConfig},
};

const PUBLISH_KEY_ENV: &str = "RUSHLS_PUBLISH_KEY";
const STREAM_ID_ENV: &str = "RUSHLS_STREAM_ID";
const RTMP_ADDRESS_ENV: &str = "RUSHLS_RTMP_LISTEN";
const HTTP_ADDRESS_ENV: &str = "RUSHLS_HTTP_LISTEN";
/// Where playlists root the names they emit, e.g. `https://cdn.example.com/hls`.
const PUBLIC_BASE_ENV: &str = "RUSHLS_PUBLIC_BASE";

struct StderrEvents;

impl EventObserver for StderrEvents {
    fn observe(&self, session: SessionId, event: SessionEvent) {
        eprintln!("session {session:?}: {event:?}");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let publish_key = env::var(PUBLISH_KEY_ENV)
        .map_err(|_| format!("{PUBLISH_KEY_ENV} must contain the RTMP publishing key"))?;
    if publish_key.is_empty() {
        return Err(format!("{PUBLISH_KEY_ENV} must not be empty").into());
    }

    let mut config = NodeConfig::default();
    config.rtmp_address = socket_address(RTMP_ADDRESS_ENV, config.rtmp_address)?;
    config.http_address = socket_address(HTTP_ADDRESS_ENV, config.http_address)?;
    // Unset leaves every playlist name relative, which is correct behind any
    // host or path prefix. Set, playlists name their resources absolutely.
    if let Ok(base) = env::var(PUBLIC_BASE_ENV) {
        config.delivery.uri_base = UriBase::new(base);
    }
    let stream_id = StreamId::new(env::var(STREAM_ID_ENV).unwrap_or_else(|_| "live/camera".into()));
    let authenticator = FixedStreamAuthenticator::new(
        publish_key,
        PublishGrant {
            stream_id: stream_id.clone(),
            principal: Principal("configured-publisher".into()),
            policy: StreamPolicy::permissive(),
        },
    );
    eprintln!(
        "RTMP listening on {}; HLS listening on {}; publishing to {stream_id}",
        config.rtmp_address, config.http_address
    );
    let node = Node::new(
        config,
        Arc::new(authenticator),
        Events::new(Arc::new(StderrEvents)),
    )?;
    node.serve(shutdown_signal()).await?;
    Ok(())
}

fn socket_address(
    variable: &'static str,
    default: SocketAddr,
) -> Result<SocketAddr, Box<dyn Error>> {
    match env::var(variable) {
        Ok(value) => value
            .parse()
            .map_err(|error| format!("{variable} is not a socket address: {error}").into()),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("could not read {variable}: {error}").into()),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler installs");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
