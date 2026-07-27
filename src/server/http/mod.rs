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
//! HTTP/1.1 and cleartext HTTP/2 are both served, negotiated per connection.
//! TLS is deliberately not terminated here. Apple's Low-Latency profile expects
//! HTTP/2, which browsers reach only over TLS with ALPN — but that is a
//! deployment concern with a well-trodden answer (a terminating proxy), and
//! coupling an origin permanently to in-process certificate management buys
//! nothing that a proxy does not already provide.

mod body;
mod route;

#[cfg(test)]
mod tests;

use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, RawQuery, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use tokio::net::TcpListener;

use crate::delivery::hls::serve::{
    Body as DeliveryBody, Caching, DeliveryError, MediaBody, Origin, Response as DeliveryResponse,
};

use body::{RangeOutcome, StoredMediaBody, parse_range};
use route::route;

/// How long a client may reuse a response.
///
/// Media is addressed by durable identity, so its bytes can never change under
/// a URL and it is cacheable for as long as anyone will keep it. A playlist
/// describes a live edge and is stale the instant it is written; `no-cache`
/// rather than `no-store` because a cache revalidating is useful and a cache
/// holding a copy for the blocked-reload round trip is exactly the point.
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
const LIVE_CACHE: &str = "no-cache";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpConfig {
    /// Rejects a connection that opens and then says nothing.
    pub header_timeout: Duration,
    /// Allows playback from a page this origin does not serve.
    pub permissive_cors: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            header_timeout: Duration::from_secs(10),
            permissive_cors: true,
        }
    }
}

#[derive(Clone)]
struct Service {
    origin: Arc<Origin>,
    config: HttpConfig,
}

/// Builds the router that serves one origin.
pub fn router(origin: Arc<Origin>, config: HttpConfig) -> Router {
    Router::new()
        // One catch-all rather than a route table: a stream identity may
        // contain slashes, so path structure is resolved by the router module
        // rather than by pattern matching.
        .route("/{*path}", any(handle))
        .fallback(any(handle))
        .with_state(Service { origin, config })
}

/// Serves until `shutdown` completes, then lets in-flight requests finish.
///
/// Graceful shutdown matters more here than in most services: a blocking
/// playlist reload is *designed* to be parked for up to three target durations,
/// and dropping those connections would turn a routine restart into a visible
/// stall for every viewer.
pub async fn serve(
    listener: TcpListener,
    origin: Arc<Origin>,
    config: HttpConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(origin, config).into_make_service())
        .with_graceful_shutdown(shutdown)
        .await
}

/// The address a bound listener is actually on.
pub async fn bind(address: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(address).await
}

async fn handle(
    State(service): State<Service>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    // HEAD is answered exactly like GET and then stripped of its body by the
    // server, so a client probing for size or existence sees the truth.
    if !matches!(method, Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }

    let routed = match route(uri.path(), query.as_deref()) {
        Ok(routed) => routed,
        Err(error) => return error_response(error, &service.config),
    };
    let response = match service.origin.serve(&routed.stream, routed.request).await {
        Ok(response) => response,
        Err(error) => return error_response(error, &service.config),
    };

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    match into_http(response, range.as_deref()) {
        Ok(response) => with_common_headers(response, &service.config),
        Err(status) => with_common_headers(status.into_response(), &service.config),
    }
}

fn into_http(response: DeliveryResponse, range: Option<&str>) -> Result<Response, StatusCode> {
    let content_type = HeaderValue::from_static(response.content_type);
    let cache = HeaderValue::from_static(match response.caching {
        Caching::Immutable => IMMUTABLE_CACHE,
        Caching::Live => LIVE_CACHE,
    });

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

/// Maps a delivery failure onto the status that describes it.
///
/// The distinctions carry real information for an operator reading logs: a
/// directive naming an impossible position is the client's mistake (400), a
/// deadline passing without the media arriving is the origin failing to keep up
/// (503), and an unknown resource is neither.
fn error_response(error: DeliveryError, config: &HttpConfig) -> Response {
    let status = match error {
        DeliveryError::UnknownStream
        | DeliveryError::UnknownRendition
        | DeliveryError::UnknownResource => StatusCode::NOT_FOUND,
        DeliveryError::InvalidDirective(_) => StatusCode::BAD_REQUEST,
        DeliveryError::Unsatisfied => StatusCode::SERVICE_UNAVAILABLE,
        DeliveryError::Projection => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = (status, error.to_string()).into_response();
    if matches!(error, DeliveryError::Unsatisfied) {
        // Tells a client to come back rather than to give up on the stream.
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    with_common_headers(response, config)
}

fn with_common_headers(mut response: Response, config: &HttpConfig) -> Response {
    if config.permissive_cors {
        let headers = response.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        // Without this a browser player cannot read Content-Length or
        // Content-Range from a cross-origin response, which is what its buffer
        // accounting runs on.
        headers.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("Content-Length, Content-Range, Date"),
        );
    }
    response
}

const _: Option<Infallible> = None;
