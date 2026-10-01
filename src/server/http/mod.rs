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
mod limits;
pub mod playback;
mod tls;

#[cfg(test)]
pub mod fixtures;
#[cfg(test)]
mod telemetry_tests;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
use rushls_common::metrics::bearer_token;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::net::TcpListener;

use crate::{
    delivery::{
        Body as DeliveryBody, DeliveryError, DeliveryFailure, MediaBody,
        Response as DeliveryResponse, Reuse,
    },
    observe::{Events, ProcessMeters, http::HttpFailure},
    server::http::playback::PlaybackDenial,
    server::metrics::MetricsEndpoint,
};

pub use cors::{AllowedOrigins, CorsConfig, OriginPattern, OriginPatternError, WildcardDepth};
pub use limits::{HttpBudget, HttpLimits};
pub use playback::{PlaybackGate, PlaybackSettings, PlaybackStartError};

use body::{RangeOutcome, StoredMediaBody, parse_range};

pub use tls::{TlsError, TlsListener, TlsSettings};
pub(crate) use tls::{rotating_ingest_server_config, rotating_quic_server_config};

/// What a response with no reusable lifetime says.
///
/// `no-cache` rather than `no-store` because revalidating is useful and a cache
/// holding a copy for the blocked-reload round trip is exactly the point.
const REVALIDATE: &str = "no-cache";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HttpConfig {
    /// Aggregate HTTP capacity, shared across this node’s listeners.
    pub limits: HttpLimits,
    /// Who may play this origin from a page it does not serve.
    pub cors: CorsConfig,
    /// Absent serves cleartext, which is the right answer behind a proxy.
    pub tls: Option<TlsSettings>,
    /// Where HTTPS is served, when TLS is configured.
    ///
    /// Beside the certificate rather than inside `TlsSettings`, which is
    /// shared with another application that binds its own listeners.
    pub tls_address: Option<SocketAddr>,
    /// The MOQ listener's certificate, when MOQ ingest is on.
    ///
    /// Present only to serve `/certificate.sha256`. A browser cannot complete
    /// a WebTransport handshake against a certificate it does not trust, and
    /// Chromium's QUIC path does not consult locally installed roots the way
    /// its TCP path does, so a development origin publishes the fingerprint
    /// its client pins with `serverCertificateHashes` instead. Absent when the
    /// listener is off, which keeps the route off a production origin whose
    /// publicly trusted certificate needs no pinning.
    pub moq_certificate: Option<PathBuf>,
}

impl HttpConfig {
    /// Rejects a configuration that cannot work, before anything binds.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.limits.validate()?;
        self.cors.validate()
    }
}

/// The application one HTTP request target is delegated to.
///
/// Owned by the transport that consumes it; implementations are composed in
/// runtime so lower layers never name the HTTP server.
pub trait Application: Send + Sync + 'static {
    /// Attribute only recognized paths belonging to retained streams. Unknown
    /// streams stay in node totals, preventing viewer-controlled cardinality.
    fn http_meters(&self, _path: &str) -> Option<crate::observe::http::HttpMeters> {
        None
    }

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
    playback: Option<Arc<PlaybackGate>>,
    /// Behind an `Arc` for the same reason as the CORS policy: cloned per
    /// request, and a `PathBuf` allocates.
    moq_certificate: Option<Arc<PathBuf>>,
}

impl<P> Clone for HttpState<P> {
    fn clone(&self) -> Self {
        Self {
            application: Arc::clone(&self.application),
            cors: Arc::clone(&self.cors),
            metrics: self.metrics.clone(),
            readiness: self.readiness.clone(),
            playback: self.playback.clone(),
            moq_certificate: self.moq_certificate.clone(),
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
    playback: Option<Arc<PlaybackGate>>,
) -> Router {
    // One catch-all rather than a route table: a stream identity may contain
    // slashes, so path structure is resolved by the application rather than by
    // axum pattern matching.
    let mut viewer = Router::new()
        .route("/{*path}", any(handle::<P>))
        .fallback(any(handle::<P>));
    // Before the CORS layer, so the fingerprint carries the same
    // `Access-Control-Allow-Origin` as the media it belongs with: the page
    // that fetches it is served from wherever a developer keeps it, not from
    // this origin. A static path outranks the catch-all in axum's router, so
    // a stream may still be named `certificate.sha256` at any other depth.
    if config.moq_certificate.is_some() {
        viewer = viewer.route("/certificate.sha256", any(moq_fingerprint::<P>));
    }
    if let Some(cors) = cors::layer(&config.cors) {
        viewer = viewer.layer(cors);
    }

    let mut operator = Router::new()
        .route("/health/live", any(liveness))
        .route("/health/ready", any(readiness_probe::<P>));
    // Left to the catch-all when disabled, so `/metrics` is an ordinary
    // unknown resource rather than a route that exists and refuses.
    if metrics.is_some() {
        operator = operator
            .route("/metrics", any(metrics_totals::<P>))
            .route("/metrics/streams", any(metrics_streams::<P>));
    }

    operator.merge(viewer).with_state(HttpState {
        application,
        cors: Arc::new(config.cors.clone()),
        metrics,
        readiness,
        playback,
        moq_certificate: config.moq_certificate.clone().map(Arc::new),
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
    playback: Option<Arc<PlaybackGate>>,
    readiness: Readiness,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    P: Application,
{
    let budget = HttpBudget::new(config.limits);
    serve_with_budget(
        listener,
        application,
        config,
        metrics,
        playback,
        readiness,
        budget,
        shutdown,
    )
    .await
}

/// Shares admission across listeners rather than multiplying capacity per bind.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_budget<L, P>(
    listener: L,
    application: Arc<P>,
    config: HttpConfig,
    metrics: Option<MetricsEndpoint>,
    playback: Option<Arc<PlaybackGate>>,
    readiness: Readiness,
    budget: HttpBudget,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    P: Application,
{
    let router = router(
        Arc::clone(&application),
        &config,
        metrics,
        readiness,
        playback,
    )
    .layer(axum::middleware::from_fn_with_state(
        (budget.clone(), application),
        limits::admit_application::<P>,
    ));
    limits::serve(listener, router, budget, shutdown).await
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
    TlsListener::new(
        listener,
        settings,
        std::sync::Arc::new(tls::NodeTlsObserver::new(meters, events)),
    )
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

    let shared = service.playback.is_none();
    if let Some(gate) = &service.playback
        && let Err(denial) = gate.authorize(uri.path(), query.as_deref(), &headers)
    {
        return playback_denied(denial);
    }

    let response = match service
        .application
        .serve(uri.path(), query.as_deref())
        .await
    {
        Ok(response) => response,
        Err(failure) => return error_response(failure, shared),
    };

    // Borrowed straight from the request headers: the response is already
    // resolved by here, so nothing else needs the header map mutably.
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let conditional = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok());
    match into_http(response, range, accepts_gzip(&headers), conditional, shared) {
        Ok(response) => response,
        Err(status) => status.into_response(),
    }
}

async fn liveness(method: Method) -> Response {
    health_response(&method, true)
}

/// The MOQ certificate's SHA-256, hex encoded, for `serverCertificateHashes`.
///
/// Read per request rather than resolved once at startup, because the
/// certificate rotates underneath a running origin. A fingerprint that
/// outlived its certificate would be worse than none: the browser pins
/// exactly what this returns, and a stale hash fails the handshake with no
/// interstitial to click through.
///
/// The value is not a secret. It is a digest of the certificate this origin
/// already presents in the clear to every peer that opens a connection.
async fn moq_fingerprint<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // Absent is unreachable while the route is only mounted with a certificate
    // configured, but answering 404 keeps that an invariant rather than a panic.
    let Some(path) = service.moq_certificate.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(fingerprint) = leaf_fingerprint(path) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(REVALIDATE)),
        ],
        fingerprint,
    )
        .into_response()
}

/// Hex SHA-256 over the DER of the first certificate in a PEM chain.
///
/// The leaf, because that is what a browser hashes: `serverCertificateHashes`
/// pins one end-entity certificate rather than a chain or an issuer.
fn leaf_fingerprint(path: &Path) -> Option<String> {
    use std::fmt::Write as _;

    let leaf = CertificateDer::from_pem_file(path).ok()?;
    let digest = ring::digest::digest(&ring::digest::SHA256, leaf.as_ref());
    let mut hex = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        write!(hex, "{byte:02x}").ok()?;
    }
    Some(hex)
}

async fn readiness_probe<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
) -> Response {
    health_response(&method, service.readiness.is_ready())
}

async fn metrics_totals<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    metrics_probe(&service, &method, &headers, MetricsEndpoint::render)
}

async fn metrics_streams<P: Application>(
    State(service): State<HttpState<P>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    metrics_probe(&service, &method, &headers, MetricsEndpoint::render_streams)
}

fn metrics_probe(
    service: &HttpState<impl Application>,
    method: &Method,
    headers: &HeaderMap,
    body: fn(&MetricsEndpoint) -> String,
) -> Response {
    match &service.metrics {
        // Unreachable: the route only exists when the endpoint does.
        None => StatusCode::NOT_FOUND.into_response(),
        Some(metrics) => metrics_response(metrics, method, headers, body),
    }
}

fn health_response(method: &Method, healthy: bool) -> Response {
    if !matches!(*method, Method::GET | Method::HEAD) {
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

fn metrics_response(
    metrics: &MetricsEndpoint,
    method: &Method,
    headers: &HeaderMap,
    body: fn(&MetricsEndpoint) -> String,
) -> Response {
    if !matches!(*method, Method::GET | Method::HEAD) {
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
        body(metrics),
    )
        .into_response()
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
    if_none_match: Option<&str>,
    shared: bool,
) -> Result<Response, StatusCode> {
    let content_type = HeaderValue::from_static(response.content_type.name());
    // Announced only where an encoding was actually available to choose. Saying
    // it on already-compressed media would split every cache entry downstream
    // for a resource that never varies — and those are the entries this origin
    // asks caches to hold for six target durations.
    let varies = response.gzip.is_some();
    // Transformation is only a hazard for a resource that has two encodings to
    // be converted between, so the request not to transform is stated exactly
    // there.
    let cache = cache_control(response.reuse, varies, shared);
    // Derived before the gzip representation is consumed below, because both
    // tags are read out of the same member's trailer.
    let identity_tag = response
        .gzip
        .as_ref()
        .and_then(|gzip| entity_tag(gzip, false));
    let encoded_tag = response
        .gzip
        .as_ref()
        .and_then(|gzip| entity_tag(gzip, true));
    // A range names bytes of the identity representation, so the two cannot be
    // combined: a range of a gzip stream describes a different resource.
    let encoded = response.gzip.filter(|_| accepts_gzip && range.is_none());

    // Which representation this request selected is what the validator has to
    // describe, so the comparison happens after negotiation rather than before.
    let selected_tag = if encoded.is_some() {
        encoded_tag.clone()
    } else {
        identity_tag.clone()
    };
    // Evaluated ahead of `Range`, which is the order RFC 9110 § 13.2.1 sets:
    // a client holding current bytes is told so, whether or not it also asked
    // for part of them.
    if let Some(tag) = selected_tag.as_ref()
        && if_none_match.is_some_and(|header| matches_etag(header, tag))
    {
        return Ok(not_modified(tag.clone(), cache, varies));
    }

    if let Some(gzip) = encoded {
        let length = gzip.len();
        let encoded = (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, cache),
                (header::CONTENT_ENCODING, HeaderValue::from_static("gzip")),
                (header::ACCEPT_RANGES, HeaderValue::from_static("none")),
            ],
            gzip,
        )
            .into_response();
        return Ok(with_vary(
            with_etag(with_content_length(encoded, length), encoded_tag),
            varies,
        ));
    }

    match response.body {
        DeliveryBody::Manifest(bytes) => {
            let length = bytes.len();
            let manifest = (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache),
                    // A playlist is rewritten in place at the live edge and is
                    // small enough that no client has reason to fetch part of
                    // one. Saying so outright, rather than ignoring `Range`
                    // silently as this branch does, denies an intermediary the
                    // premise it needs to invent a partial response.
                    (header::ACCEPT_RANGES, HeaderValue::from_static("none")),
                ],
                bytes,
            )
                .into_response();
            Ok(with_vary(
                with_etag(with_content_length(manifest, length), identity_tag),
                varies,
            ))
        }
        DeliveryBody::Media(media) => {
            let length = media.len();
            let Some(range) = range else {
                return Ok(with_vary(
                    with_etag(
                        media_response(media, content_type, cache, None, length),
                        identity_tag,
                    ),
                    varies,
                ));
            };
            match parse_range(range, length) {
                RangeOutcome::Ignore => Ok(with_vary(
                    with_etag(
                        media_response(media, content_type, cache, None, length),
                        identity_tag,
                    ),
                    varies,
                )),
                RangeOutcome::Unsatisfiable => Err(StatusCode::RANGE_NOT_SATISFIABLE),
                RangeOutcome::Satisfiable(range) => {
                    // Disk frames never belong in a Chunked body; the origin
                    // loads them first. Failing closed beats a shifted 206.
                    let clipped = media
                        .range(range.start, range.end)
                        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
                    Ok(with_vary(
                        with_etag(
                            media_response(
                                clipped,
                                content_type,
                                cache,
                                Some((range.start, range.end)),
                                length,
                            ),
                            // The same representation, so the same validator: a
                            // range does not name a resource of its own.
                            identity_tag,
                        ),
                        varies,
                    ))
                }
            }
        }
    }
}

/// A strong entity tag, taken from the gzip member this origin already built.
///
/// RFC 9110 § 8.8.3 requires two representations of one resource to carry
/// different entity tags, and a shared cache that cannot tell them apart is
/// free to answer a request for one with the other — which is the whole of the
/// failure this guards against, since an identity request answered from a gzip
/// entry is how a playlist arrives with a length that describes different
/// bytes than the body does.
///
/// The value costs nothing to produce. A gzip member ends with a CRC32 of the
/// uncompressed bytes followed by their length (RFC 1952 § 2.3.1), so the
/// validator for the identity representation has already been computed inside
/// the render cache's critical section. Hashing the playlist again per request
/// would put work on the request path that this origin deliberately keeps off
/// it.
fn entity_tag(gzip: &bytes::Bytes, encoded: bool) -> Option<HeaderValue> {
    let trailer = gzip.get(gzip.len().checked_sub(8)?..)?;
    let crc = u32::from_le_bytes(trailer[..4].try_into().ok()?);
    // Truncated to 32 bits by the format itself, which is why the tag pairs it
    // with the CRC rather than trusting either alone.
    let size = u32::from_le_bytes(trailer[4..].try_into().ok()?);
    // The suffix is what keeps the two encodings distinguishable; without it
    // both representations would validate as the same entity.
    let suffix = if encoded { "-gz" } else { "" };
    HeaderValue::from_str(&format!("\"{crc:08x}-{size:x}{suffix}\"")).ok()
}

/// Attaches a validator, where one could be derived.
fn with_etag(mut response: Response, tag: Option<HeaderValue>) -> Response {
    if let Some(tag) = tag {
        response.headers_mut().insert(header::ETAG, tag);
    }
    response
}

/// Answers a client whose copy is still current.
///
/// Worth having on this origin specifically: a playlist is reusable for half a
/// target duration, so a viewer revalidates it every few seconds for the whole
/// session, and the bytes are identical across the overwhelming majority of
/// those reloads. Carries no body and no `Content-Length` — a 304 is framed by
/// its status, and a length here is the kind of thing an intermediary converts
/// into a body that is not there.
fn not_modified(tag: HeaderValue, cache: HeaderValue, varies: bool) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    // `into_response` gives an empty body a zero length; a 304 must carry
    // neither.
    let headers = response.headers_mut();
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::ETAG, tag);
    headers.insert(header::CACHE_CONTROL, cache);
    with_vary(response, varies)
}

/// Weak comparison of an `If-None-Match` list against one entity tag.
///
/// Weak rather than strong because that is what RFC 9110 § 13.1.2 specifies for
/// this field: the question is whether the representations are equivalent for
/// caching, not whether they are byte-identical. In practice it means a cache
/// that stored `"abc"` and revalidates with `W/"abc"` is answered correctly.
fn matches_etag(header: &str, tag: &HeaderValue) -> bool {
    let Ok(tag) = tag.to_str() else {
        return false;
    };
    let opaque = |value: &str| {
        value
            .trim()
            .strip_prefix("W/")
            .unwrap_or_else(|| value.trim())
            .to_owned()
    };
    let current = opaque(tag);
    header
        .split(',')
        // `*` matches whenever a representation exists at all, which by the
        // time a response has been produced it does.
        .any(|candidate| candidate.trim() == "*" || opaque(candidate) == current)
}

/// Resolves a lifetime into the header, asking negotiated text be left alone.
///
/// `no-transform` is the standards-defined way (RFC 9111 § 5.2.2.6) to tell an
/// intermediary not to convert between content codings. It is stated only for
/// resources that have more than one coding, because it is meaningless — and
/// on media, misleading — anywhere else.
///
/// `public` is omitted when playback authorization is on. RFC 9111 § 3.5 says
/// that directive waives the default restriction on storing a response to a
/// request that carried `Authorization`, which would let a CDN serve one
/// viewer's playlist to another.
fn cache_control(reuse: Reuse, negotiated: bool, shared: bool) -> HeaderValue {
    let value = reuse_header(reuse, shared);
    if !negotiated {
        return value;
    }
    value
        .to_str()
        .ok()
        .and_then(|directives| HeaderValue::try_from(format!("{directives}, no-transform")).ok())
        .unwrap_or(value)
}

/// Writes a resolved lifetime as the header that communicates it.
///
/// Whole seconds is all the header carries, so a lifetime shorter than one
/// becomes a revalidation. Truncating errs toward asking again sooner than
/// policy allows rather than later, which is the safe direction for a live
/// edge.
fn reuse_header(reuse: Reuse, shared: bool) -> HeaderValue {
    let seconds = reuse.max_age.as_secs();
    if seconds == 0 {
        return HeaderValue::from_static(REVALIDATE);
    }
    let mut value = if shared {
        format!("public, max-age={seconds}")
    } else {
        format!("max-age={seconds}")
    };
    if reuse.immutable {
        value.push_str(", immutable");
    }
    HeaderValue::try_from(value).unwrap_or(HeaderValue::from_static(REVALIDATE))
}

/// Declares the length of a body that is already whole in memory.
///
/// Stated outright rather than left to the body's size hint, because a response
/// that reaches the wire without one is framed as `Transfer-Encoding: chunked`.
/// That is legal HTTP and irrelevant over HTTP/2, but clients that reissue
/// requests on a kept-alive HTTP/1.1 connection mis-frame everything after the
/// first chunked response — libavformat's HLS reader does exactly this by
/// default, and reads the second playlist of a session as garbage.
fn with_content_length(mut response: Response, length: usize) -> Response {
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
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

/// Maps a delivery failure onto the status that describes it.
///
/// The distinctions carry real information for an operator reading logs: a
/// directive naming an impossible position is the client's mistake (400), a
/// deadline passing without the media arriving is the origin failing to keep up
/// (503), and an unknown resource is neither.
fn error_response(failure: DeliveryFailure, shared: bool) -> Response {
    let status = match failure.error {
        DeliveryError::UnknownStream
        | DeliveryError::UnknownRendition
        | DeliveryError::UnknownResource => StatusCode::NOT_FOUND,
        DeliveryError::InvalidDirective(_) => StatusCode::BAD_REQUEST,
        DeliveryError::Unsatisfied => StatusCode::SERVICE_UNAVAILABLE,
        DeliveryError::Projection => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = (status, failure.error.to_string()).into_response();
    response.extensions_mut().insert(match failure.error {
        DeliveryError::UnknownStream => HttpFailure::UnknownStream,
        DeliveryError::UnknownRendition => HttpFailure::UnknownRendition,
        DeliveryError::UnknownResource => HttpFailure::UnknownResource,
        DeliveryError::InvalidDirective(_) => HttpFailure::InvalidDirective,
        DeliveryError::Unsatisfied => HttpFailure::Unsatisfied,
        DeliveryError::Projection => HttpFailure::Projection,
    });
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, reuse_header(failure.reuse, shared));
    if matches!(failure.error, DeliveryError::Unsatisfied) {
        // Tells a client to come back rather than to give up on the stream.
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    // Errors carry the same CORS policy as successes — a player that cannot
    // read a 404 cross-origin sees a network failure instead, and retries
    // forever — which the layer now applies to every response alike.
    response
}

/// A viewer JWT that was missing, malformed, or not admitted for this stream.
fn playback_denied(denial: PlaybackDenial) -> Response {
    let mut response = denial.status().into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if matches!(denial, PlaybackDenial::Unauthorized) {
        headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}
