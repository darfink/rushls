use std::{env, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

use rushls::{
    admission::{FixedStreamAuthenticator, Principal, PublishGrant, StreamPolicy},
    delivery::hls::uri::UriBase,
    domain::{SessionId, StreamId},
    observe::{EventObserver, Events, NodeEvent, SessionEvent},
    server::{AllowedOrigins, CorsConfig, Node, NodeConfig, TlsSettings},
    source::transport::srt::{SrtEncryption, SrtKeyLength},
};

const PUBLISH_KEY_ENV: &str = "RUSHLS_PUBLISH_KEY";
const STREAM_ID_ENV: &str = "RUSHLS_STREAM_ID";
const RTMP_ADDRESS_ENV: &str = "RUSHLS_RTMP_LISTEN";
const SRT_ADDRESS_ENV: &str = "RUSHLS_SRT_LISTEN";
const SRT_PASSPHRASE_ENV: &str = "RUSHLS_SRT_PASSPHRASE";
const HTTP_ADDRESS_ENV: &str = "RUSHLS_HTTP_LISTEN";
/// PEM, leaf first. Set together with the key to serve HTTPS directly.
const TLS_CERTIFICATE_ENV: &str = "RUSHLS_TLS_CERT";
const TLS_KEY_ENV: &str = "RUSHLS_TLS_KEY";
/// Where playlists root the names they emit, e.g. `https://cdn.example.com/hls`.
const PUBLIC_BASE_ENV: &str = "RUSHLS_PUBLIC_BASE";
/// Comma-separated origins, or `*`. Unset allows any, which suits a public
/// origin; `off` serves no access-control headers at all.
const CORS_ORIGINS_ENV: &str = "RUSHLS_CORS_ORIGINS";
/// Lets a player send cookies. Requires an explicit origin allowlist.
const CORS_CREDENTIALS_ENV: &str = "RUSHLS_CORS_CREDENTIALS";

struct StderrEvents;

impl EventObserver for StderrEvents {
    fn observe(&self, session: SessionId, event: SessionEvent) {
        eprintln!("session {session:?}: {event:?}");
    }

    fn observe_node(&self, event: NodeEvent) {
        match event {
            NodeEvent::ListenerBound { protocol, address } => {
                eprintln!("{protocol} listening on {address}");
            }
            NodeEvent::CertificateLoaded { certificate } => {
                eprintln!("serving the certificate at {}", certificate.display());
            }
            NodeEvent::CertificateRejected {
                certificate,
                reason,
            } => eprintln!(
                "keeping the previous certificate; {} was rejected: {reason}",
                certificate.display()
            ),
            NodeEvent::CertificateWatchLost { reason } => {
                eprintln!("certificate rotations will no longer be noticed: {reason}");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let publish_key = env::var(PUBLISH_KEY_ENV)
        .map_err(|_| format!("{PUBLISH_KEY_ENV} must contain the publishing key"))?;
    if publish_key.is_empty() {
        return Err(format!("{PUBLISH_KEY_ENV} must not be empty").into());
    }

    let mut config = NodeConfig::default();
    config.rtmp_address = socket_address(RTMP_ADDRESS_ENV, config.rtmp_address)?;
    config.srt_address = socket_address(SRT_ADDRESS_ENV, config.srt_address)?;
    config.http_address = socket_address(HTTP_ADDRESS_ENV, config.http_address)?;
    if let Ok(passphrase) = env::var(SRT_PASSPHRASE_ENV) {
        config.srt.encryption = Some(SrtEncryption::new(passphrase, SrtKeyLength::Aes256)?);
    }
    config.http.tls = tls_settings()?;
    config.http.cors = cors_config()?;
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
    eprintln!("publishing to {stream_id}");
    let node = Node::new(
        config,
        Arc::new(authenticator),
        Events::new(Arc::new(StderrEvents)),
    )?;
    node.serve(shutdown_signal()).await?;
    Ok(())
}

/// Reads the cross-origin policy, defaulting to the public-origin answer.
fn cors_config() -> Result<CorsConfig, Box<dyn Error>> {
    let allowed_origins = match env::var(CORS_ORIGINS_ENV).ok().as_deref() {
        None | Some("*") => AllowedOrigins::Any,
        Some("off") => AllowedOrigins::Disabled,
        Some(list) => {
            let origins: Vec<String> = list
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(str::to_owned)
                .collect();
            if origins.is_empty() {
                return Err(format!("{CORS_ORIGINS_ENV} names no origins").into());
            }
            AllowedOrigins::Only(origins)
        }
    };
    let allow_credentials = match env::var(CORS_CREDENTIALS_ENV).ok().as_deref() {
        None | Some("0") | Some("false") => false,
        Some("1") | Some("true") => true,
        Some(value) => {
            return Err(format!("{CORS_CREDENTIALS_ENV} is not a boolean: {value}").into());
        }
    };
    let config = CorsConfig {
        allowed_origins,
        allow_credentials,
        ..CorsConfig::default()
    };
    config.validate()?;
    Ok(config)
}

/// Both paths or neither.
///
/// Refusing the half-configured case rather than silently falling back to
/// cleartext: an operator who set one of the two believes the origin is
/// serving HTTPS, and quietly proving them wrong is worse than not starting.
fn tls_settings() -> Result<Option<TlsSettings>, Box<dyn Error>> {
    let certificate = env::var(TLS_CERTIFICATE_ENV).ok();
    let key = env::var(TLS_KEY_ENV).ok();
    match (certificate, key) {
        (Some(certificate), Some(key)) => Ok(Some(TlsSettings {
            certificate: PathBuf::from(certificate),
            key: PathBuf::from(key),
            ..TlsSettings::default()
        })),
        (None, None) => Ok(None),
        (Some(_), None) => {
            Err(format!("{TLS_CERTIFICATE_ENV} is set without {TLS_KEY_ENV}").into())
        }
        (None, Some(_)) => {
            Err(format!("{TLS_KEY_ENV} is set without {TLS_CERTIFICATE_ENV}").into())
        }
    }
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
