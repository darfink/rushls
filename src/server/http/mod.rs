//! The HTTP surface viewers fetch from.
//!
//! Thin by construction. An [`Application`] decides what a request means,
//! whether it must wait, and what bytes answer it. This layer only translates
//! that result into HTTP and owns the concerns that genuinely belong to the
//! transport: methods, byte ranges, headers, and the listener's lifetime.
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
mod tls;

#[cfg(test)]
pub mod fixtures;
#[cfg(test)]
mod tests;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
    delivery::{
        Body as DeliveryBody, DeliveryError, DeliveryFailure, MediaBody,
        Response as DeliveryResponse, Reuse,
    },
    observe::{Events, ProcessMeters},
    server::metrics::MetricsEndpoint,
};

pub use cors::{AllowedOrigins, CorsConfig, OriginPattern, OriginPatternError, WildcardDepth};

use body::{RangeOutcome, StoredMediaBody, parse_range};

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

/// The application one HTTP request target is delegated to.
///
/// Owned by the transport that consumes it; implementations are composed in
/// runtime so lower layers never name the HTTP server.
pub trait Application: Send + Sync + 'static {
    fn serve<'a>(
        &'a self,
        path: &'a str,
        query: Option<&'a str>,
    ) -> impl Future<Output = Result<DeliveryResponse, DeliveryFailure>> + Send + 'a;
}

/// Per-request state: the application and HTTP-only policy.
///
/// The CORS policy is behind an `Arc` because axum clones this for every
/// request and an allowlist is a `Vec<String>`; the certificate paths are not
/// here at all, for the same reason.
struct HttpState<P> {
    application: Arc<P>,
    cors: Arc<CorsConfig>,
    metrics: Option<MetricsEndpoint>,
    readiness: Readiness,
}

impl<P> Clone for HttpState<P> {
    fn clone(&self) -> Self {
        Self {
            application: Arc::clone(&self.application),
            cors: Arc::clone(&self.cors),
            metrics: self.metrics.clone(),
            readiness: self.readiness.clone(),
        }
    }
}

/// Whether this node should receive new traffic.
///
/// Liveness needs no mutable state: successfully handling its request already
/// proves that the HTTP task and runtime are responsive. Readiness differs
/// during startup and graceful shutdown, so process wiring owns this latch.
#[derive(Clone, Debug, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    pub fn ready() -> Self {
        let readiness = Self::default();
        readiness.mark_ready();
        readiness
    }

    pub fn mark_ready(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn mark_not_ready(&self) {
        self.0.store(false, Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Builds the router that serves one origin, including the operator surface.
///
/// Cross-origin access is applied to the viewer surface alone. The health
/// probes and `/metrics` are for the operator's own infrastructure, and giving
/// them `Access-Control-Allow-Origin` would let any page a browser happens to
/// load read them — which for an unauthenticated `/metrics` means publishing
/// stream names to anyone who can get script into a viewer's browser.
fn router<P: Application>(
    application: Arc<P>,
    config: &HttpConfig,
    metrics: Option<MetricsEndpoint>,
    readiness: Readiness,
) -> Router {
    // One catch-all rather than a route table: a stream identity may contain
    // slashes, so path structure is resolved by the application rather than by
    // axum pattern matching.
    let mut viewer = Router::new()
        .route("/{*path}", any(handle::<P>))
        .fallback(any(handle::<P>));
    if let Some(cors) = cors::layer(&config.cors) {
        viewer = viewer.layer(cors);
    }

    let mut operator = Router::new()
        .route("/health/live", any(liveness))
        .route("/health/ready", any(readiness_probe::<P>));
    // Left to the catch-all when disabled, so `/metrics` is an ordinary
    // unknown resource rather than a route that exists and refuses.
    if metrics.is_some() {
        operator = operator.route("/metrics", any(metrics_probe::<P>));
    }

    operator.merge(viewer).with_state(HttpState {
        application,
        cors: Arc::new(config.cors.clone()),
        metrics,
        readiness,
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
pub async fn serve<L, P>(
    listener: L,
    application: Arc<P>,
    config: HttpConfig,
    metrics: Option<MetricsEndpoint>,
    readiness: Readiness,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    P: Application,
{
    axum::serve(
        listener,
        router(application, &config, metrics, readiness).into_make_service(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Binds and terminates TLS, loading the certificate and starting its watch.
///
/// Separate from binding the socket rather than folded into it because binding
/// can fail for reasons a certificate cannot, and an operator reading a startup
/// failure deserves to know which of the two went wrong.
pub fn bind_tls(
    listener: TcpListener,
    settings: TlsSettings,
    meters: ProcessMeters,
    events: Events,
) -> Result<TlsListener, TlsError> {
    TlsListener::new(listener, settings, meters, events)
}

async fn handle<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    // HEAD is answered exactly like GET and then stripped of its body by the
    // server, so a client probing for size or existence sees the truth.
    //
    // A preflight never arrives here: the CORS layer answers `OPTIONS` itself.
    if !matches!(method, Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, cors::ALLOWED_METHODS)],
        )
            .into_response();
    }

    let response = match service
        .application
        .serve(uri.path(), query.as_deref())
        .await
    {
        Ok(response) => response,
        Err(failure) => return error_response(failure),
    };

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    match into_http(response, range.as_deref(), accepts_gzip(&headers)) {
        Ok(response) => response,
        Err(status) => status.into_response(),
    }
}

async fn liveness(method: Method) -> Response {
    health_response(method, true)
}

async fn readiness_probe<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
) -> Response {
    health_response(method, service.readiness.is_ready())
}

async fn metrics_probe<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    match &service.metrics {
        // Unreachable: the route only exists when the endpoint does.
        None => StatusCode::NOT_FOUND.into_response(),
        Some(metrics) => metrics_response(metrics, method, &headers),
    }
}

fn health_response(method: Method, healthy: bool) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
        )
            .into_response();
    }

    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        if healthy { "ok\n" } else { "not ready\n" },
    )
        .into_response()
}

fn metrics_response(metrics: &MetricsEndpoint, method: Method, headers: &HeaderMap) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
        )
            .into_response();
    }

    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token);
    if !metrics.authorize(presented) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
        )
            .into_response();
    }

    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        metrics.render(),
    )
        .into_response()
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Whether the client is prepared to accept gzip.
///
/// `q=0` is a refusal rather than a preference, so an explicit `gzip;q=0` wins
/// over a wildcard that would otherwise allow it. Absent the header entirely,
/// the answer is no: HLS asks servers to compress text *if the client indicates
/// that it is prepared to accept it*, and silence is not an indication.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let mut wildcard = false;
    for entry in value.split(',') {
        let mut parts = entry.split(';');
        let coding = parts.next().unwrap_or_default().trim();
        let acceptable = !parts.any(|parameter| {
            parameter
                .trim()
                .strip_prefix("q=")
                .and_then(|quality| quality.trim().parse::<f32>().ok())
                .is_some_and(|quality| quality <= 0.0)
        });
        if coding.eq_ignore_ascii_case("gzip") {
            return acceptable;
        }
        if coding == "*" {
            wildcard = acceptable;
        }
    }
    wildcard
}

fn into_http(
    response: DeliveryResponse,
    range: Option<&str>,
    accepts_gzip: bool,
) -> Result<Response, StatusCode> {
    let content_type = HeaderValue::from_static(response.content_type.name());
    let cache = response.reuse.into();
    // Announced only where an encoding was actually available to choose. Saying
    // it on already-compressed media would split every cache entry downstream
    // for a resource that never varies — and those are the entries this origin
    // asks caches to hold for six target durations.
    let varies = response.gzip.is_some();
    // A range names bytes of the identity representation, so the two cannot be
    // combined: a range of a gzip stream describes a different resource.
    let encoded = response.gzip.filter(|_| accepts_gzip && range.is_none());

    if let Some(gzip) = encoded {
        let length = gzip.len();
        let mut encoded = (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, cache),
                (header::CONTENT_ENCODING, HeaderValue::from_static("gzip")),
            ],
            gzip,
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
            encoded.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
        return Ok(with_vary(encoded, varies));
    }

    match response.body {
        DeliveryBody::Manifest(bytes) => Ok(with_vary(
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache),
                ],
                bytes,
            )
                .into_response(),
            varies,
        )),
        DeliveryBody::Media(media) => {
            let length = media.len();
            let Some(range) = range else {
                return Ok(with_vary(
                    media_response(media, content_type, cache, None, length),
                    varies,
                ));
            };
            match parse_range(range, length) {
                RangeOutcome::Ignore => Ok(with_vary(
                    media_response(media, content_type, cache, None, length),
                    varies,
                )),
                RangeOutcome::Unsatisfiable => Err(StatusCode::RANGE_NOT_SATISFIABLE),
                RangeOutcome::Satisfiable(range) => {
                    let clipped = media.range(range.start, range.end);
                    Ok(with_vary(
                        media_response(
                            clipped,
                            content_type,
                            cache,
                            Some((range.start, range.end)),
                            length,
                        ),
                        varies,
                    ))
                }
            }
        }
    }
}

/// Tells caches that this resource is negotiated, where it actually is.
fn with_vary(mut response: Response, varies: bool) -> Response {
    if varies {
        cors::append_vary(response.headers_mut(), "Accept-Encoding");
    }
    response
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
impl From<Reuse> for HeaderValue {
    fn from(reuse: Reuse) -> Self {
        let seconds = reuse.max_age.as_secs();
        if seconds == 0 {
            return Self::from_static(REVALIDATE);
        }
        let mut value = format!("public, max-age={seconds}");
        if reuse.immutable {
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
fn error_response(failure: DeliveryFailure) -> Response {
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
    headers.insert(header::CACHE_CONTROL, failure.reuse.into());
    if matches!(failure.error, DeliveryError::Unsatisfied) {
        // Tells a client to come back rather than to give up on the stream.
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    // Errors carry the same CORS policy as successes — a player that cannot
    // read a 404 cross-origin sees a network failure instead, and retries
    // forever — which the layer now applies to every response alike.
    response
}
