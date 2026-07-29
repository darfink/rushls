//! The HTTP surface viewers fetch from.
//!
//! Thin by construction. Every decision about what a request means, whether it
//! must wait, and what bytes answer it belongs to
//! [`delivery::hls::serve`](crate::delivery::hls::serve); this layer only
//! translates between that vocabulary and HTTP's, and owns the concerns that
//! genuinely are HTTP: methods, byte ranges, cache directives, and the
//! listener's lifetime.
//!
//! # Transport
//!
//! HTTP/1.1 and HTTP/2 are both served, negotiated per connection: by preface
//! detection in cleartext, by ALPN under TLS. A terminating proxy remains the
//! ordinary deployment and nothing here assumes otherwise — but Apple's
//! Low-Latency profile expects HTTP/2, which a browser reaches only over TLS,
//! and requiring a second process to put a stream on a page is a poor default
//! for an origin that is otherwise self-contained. So TLS is terminated here
//! when configured, and left alone when it is not.
//!
//! What made this worth doing rather than merely possible is that certificates
//! rotate without a restart; see [`tls`] for how, and for why the handshake
//! does not happen on the accept path.

mod body;
mod cors;
mod route;
mod tls;

#[cfg(test)]
pub mod fixtures;
#[cfg(test)]
mod tests;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, RawQuery, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use tokio::net::TcpListener;

use crate::{
    delivery::hls::{
        cache_control::CacheControl,
        serve::{
            Body as DeliveryBody, DeliveryError, DeliveryFailure, MediaBody, Origin,
            Response as DeliveryResponse,
        },
    },
    observe::{Events, ProcessMeters},
};

pub use cors::{AllowedOrigins, CorsConfig};

use body::{RangeOutcome, StoredMediaBody, parse_range};
use route::route;

pub use tls::{TlsError, TlsListener, TlsSettings};

/// What a response with no reusable lifetime says.
///
/// `no-cache` rather than `no-store` because revalidating is useful and a cache
/// holding a copy for the blocked-reload round trip is exactly the point.
const REVALIDATE: &str = "no-cache";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HttpConfig {
    /// Who may play this origin from a page it does not serve.
    pub cors: CorsConfig,
    /// Absent serves cleartext, which is the right answer behind a proxy.
    pub tls: Option<TlsSettings>,
}

impl HttpConfig {
    /// Rejects a configuration that cannot work, before anything binds.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.cors.validate()
    }
}

/// Per-request state: the origin, and the policy answering a request needs.
///
/// The CORS policy is behind an `Arc` because axum clones this for every
/// request and an allowlist is a `Vec<String>`; the certificate paths are not
/// here at all, for the same reason.
#[derive(Clone)]
struct Service {
    origin: Arc<Origin>,
    cors: Arc<CorsConfig>,
}

/// Builds the router that serves one origin.
pub fn router(origin: Arc<Origin>, config: &HttpConfig) -> Router {
    Router::new()
        // One catch-all rather than a route table: a stream identity may
        // contain slashes, so path structure is resolved by the router module
        // rather than by pattern matching.
        .route("/{*path}", any(handle))
        .fallback(any(handle))
        .with_state(Service {
            origin,
            cors: Arc::new(config.cors.clone()),
        })
}

/// Serves until `shutdown` completes, then lets in-flight requests finish.
///
/// Graceful shutdown matters more here than in most services: a blocking
/// playlist reload is *designed* to be parked for up to three target durations,
/// and dropping those connections would turn a routine restart into a visible
/// stall for every viewer.
///
/// Generic over the listener so cleartext and TLS share one server and one
/// shutdown path; only the bytes on the wire differ.
pub async fn serve<L>(
    listener: L,
    origin: Arc<Origin>,
    config: HttpConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    axum::serve(listener, router(origin, &config).into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
}

/// The address a bound listener is actually on.
pub async fn bind(address: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(address).await
}

/// Binds and terminates TLS, loading the certificate and starting its watch.
///
/// Separate from [`bind`] rather than folded into it because binding can fail
/// for reasons a certificate cannot, and an operator reading a startup failure
/// deserves to know which of the two went wrong.
pub fn bind_tls(
    listener: TcpListener,
    settings: TlsSettings,
    meters: ProcessMeters,
    events: Events,
) -> Result<TlsListener, TlsError> {
    TlsListener::new(listener, settings, meters, events)
}

async fn handle(
    State(service): State<Service>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    // A preflight is answered by policy alone and never reaches the origin:
    // the browser is asking what it may send, not for any media.
    if method == Method::OPTIONS
        && let Some(response) = cors::preflight(&service.cors, &headers)
    {
        return response;
    }

    // HEAD is answered exactly like GET and then stripped of its body by the
    // server, so a client probing for size or existence sees the truth.
    if !matches!(method, Method::GET | Method::HEAD) {
        let mut response = (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, cors::ALLOWED_METHODS)],
        )
            .into_response();
        cors::apply(&mut response, &service.cors, &headers);
        return response;
    }

    let routed = match route(uri.path(), query.as_deref()) {
        Ok(routed) => routed,
        Err(error) => {
            return error_response(service.origin.unrouted(error), &service.cors, &headers);
        }
    };
    let response = match service.origin.serve(&routed.stream, routed.request).await {
        Ok(response) => response,
        Err(failure) => return error_response(failure, &service.cors, &headers),
    };

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut response = match into_http(response, range.as_deref()) {
        Ok(response) => response,
        Err(status) => status.into_response(),
    };
    cors::apply(&mut response, &service.cors, &headers);
    response
}

fn into_http(response: DeliveryResponse, range: Option<&str>) -> Result<Response, StatusCode> {
    let content_type = HeaderValue::from_static(response.content_type);
    let cache = response.cache_control.into();

    match response.body {
        DeliveryBody::Playlist(bytes) => Ok((
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, cache),
            ],
            bytes,
        )
            .into_response()),
        DeliveryBody::Media(media) => {
            let length = media.len();
            let Some(range) = range else {
                return Ok(media_response(media, content_type, cache, None, length));
            };
            match parse_range(range, length) {
                RangeOutcome::Ignore => {
                    Ok(media_response(media, content_type, cache, None, length))
                }
                RangeOutcome::Unsatisfiable => Err(StatusCode::RANGE_NOT_SATISFIABLE),
                RangeOutcome::Satisfiable(range) => {
                    let clipped = media.range(range.start, range.end);
                    Ok(media_response(
                        clipped,
                        content_type,
                        cache,
                        Some((range.start, range.end)),
                        length,
                    ))
                }
            }
        }
    }
}

fn media_response(
    media: MediaBody,
    content_type: HeaderValue,
    cache: HeaderValue,
    range: Option<(u64, u64)>,
    total: u64,
) -> Response {
    let length = media.len();
    let mut response = Response::new(Body::new(StoredMediaBody::new(media)));
    if range.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CACHE_CONTROL, cache);
    // Advertised so a client can tell a truncated transfer from a short
    // resource, which for a partial segment is the difference between playing
    // and stalling.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    if let Some((start, end)) = range
        && let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
    {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}

/// Writes a resolved lifetime as the header that communicates it.
///
/// Whole seconds is all the header carries, so a lifetime shorter than one
/// becomes a revalidation. Truncating errs toward asking again sooner than
/// policy allows rather than later, which is the safe direction for a live
/// edge.
impl From<CacheControl> for HeaderValue {
    fn from(cache_control: CacheControl) -> Self {
        let seconds = cache_control.max_age.as_secs();
        if seconds == 0 {
            return Self::from_static(REVALIDATE);
        }
        let mut value = format!("public, max-age={seconds}");
        if cache_control.immutable {
            value.push_str(", immutable");
        }
        Self::try_from(value).unwrap_or(Self::from_static(REVALIDATE))
    }
}

/// Maps a delivery failure onto the status that describes it.
///
/// The distinctions carry real information for an operator reading logs: a
/// directive naming an impossible position is the client's mistake (400), a
/// deadline passing without the media arriving is the origin failing to keep up
/// (503), and an unknown resource is neither.
fn error_response(failure: DeliveryFailure, cors: &CorsConfig, request: &HeaderMap) -> Response {
    let status = match failure.error {
        DeliveryError::UnknownStream
        | DeliveryError::UnknownRendition
        | DeliveryError::UnknownResource => StatusCode::NOT_FOUND,
        DeliveryError::InvalidDirective(_) => StatusCode::BAD_REQUEST,
        DeliveryError::Unsatisfied => StatusCode::SERVICE_UNAVAILABLE,
        DeliveryError::Projection => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = (status, failure.error.to_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, failure.cache_control.into());
    if matches!(failure.error, DeliveryError::Unsatisfied) {
        // Tells a client to come back rather than to give up on the stream.
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    // Errors carry the same policy as successes: a player that cannot read a
    // 404 cross-origin sees a network failure instead, and retries forever.
    cors::apply(&mut response, cors, request);
    response
}
