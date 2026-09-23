//! Driven against a real listener, for the same reason the HTTP server is:
//! deadlines, body limits, and header handling only exist once bytes are
//! actually on a socket.

use std::{net::SocketAddr, time::Duration};

use axum::{
    Router,
    extract::Request,
    response::IntoResponse,
    routing::{get, post},
};
use bytes::Bytes;
use http::{StatusCode, header};

use super::{BearerToken, ClientConfig, Endpoint, EndpointError, HttpClient, OutboundError};

const JSON: &str = "application/json";

/// Echoes back what it was sent, so one route covers every request assertion.
async fn echo(request: Request) -> impl IntoResponse {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<absent>")
        .to_owned();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<absent>")
        .to_owned();
    let method = request.method().to_string();
    format!("{method} {content_type} {authorization}")
}

async fn slow() -> impl IntoResponse {
    tokio::time::sleep(Duration::from_secs(30)).await;
    "never arrives"
}

async fn enormous() -> impl IntoResponse {
    vec![b'x'; 256 * 1024]
}

async fn teapot() -> impl IntoResponse {
    (StatusCode::IM_A_TEAPOT, "no coffee")
}

async fn busy() -> impl IntoResponse {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "5")],
        "slow down",
    )
}

/// The HTTP-date form, which is legal and deliberately not supported.
async fn dated() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "Wed, 21 Oct 2026 07:28:00 GMT")],
        "later",
    )
}

async fn keys() -> impl IntoResponse {
    r#"{"keys":[]}"#
}

/// Starts a server on an ephemeral loopback port and returns its address.
async fn start() -> SocketAddr {
    let router = Router::new()
        .route("/echo", post(echo))
        .route("/slow", post(slow))
        .route("/enormous", post(enormous))
        .route("/teapot", post(teapot))
        .route("/busy", post(busy))
        .route("/dated", post(dated))
        .route("/keys", get(keys));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port is available");
    let address = listener.local_addr().expect("the listener is bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    address
}

fn client(config: ClientConfig) -> HttpClient {
    HttpClient::new(config).expect("a client builds from the platform roots")
}

fn endpoint(address: SocketAddr, path: &str) -> Endpoint {
    Endpoint::parse(&format!("http://{address}{path}")).expect("a loopback URL is accepted")
}

#[tokio::test]
async fn a_request_carries_its_content_type_and_bearer_credential() {
    let address = start().await;
    let client = client(ClientConfig::default());
    let token = BearerToken::new("sidecar-secret").expect("a printable token is valid");

    let response = client
        .post(
            &endpoint(address, "/echo"),
            JSON,
            Some(&token),
            Bytes::from_static(b"{}"),
        )
        .await
        .expect("the endpoint answers");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "POST application/json Bearer sidecar-secret"
    );
}

#[tokio::test]
async fn a_status_is_reported_rather_than_interpreted() {
    let address = start().await;

    let response = client(ClientConfig::default())
        .post(&endpoint(address, "/teapot"), JSON, None, Bytes::new())
        .await
        .expect("a non-2xx status is still an answer");

    assert_eq!(
        response.status,
        StatusCode::IM_A_TEAPOT,
        "success, retryable, and permanent are the caller's policy: admission \
         and hooks classify the same status differently"
    );
    assert_eq!(response.body, Bytes::from_static(b"no coffee"));
}

#[tokio::test]
async fn a_get_fetches_without_a_body_or_content_type() {
    let address = start().await;

    let response = client(ClientConfig::default())
        .get(&endpoint(address, "/keys"))
        .await
        .expect("the endpoint answers");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, Bytes::from_static(br#"{"keys":[]}"#));
}

#[tokio::test]
async fn a_slow_endpoint_hits_the_request_deadline() {
    let address = start().await;
    let client = client(ClientConfig {
        request_timeout: Duration::from_millis(150),
        ..ClientConfig::default()
    });

    let error = client
        .post(&endpoint(address, "/slow"), JSON, None, Bytes::new())
        .await
        .expect_err("an endpoint that never answers must not hold the caller");

    assert_eq!(error, OutboundError::Timeout);
}

#[tokio::test]
async fn an_oversized_response_is_refused_while_it_is_read() {
    let address = start().await;
    let client = client(ClientConfig {
        maximum_response_bytes: 1_024,
        ..ClientConfig::default()
    });

    let error = client
        .post(&endpoint(address, "/enormous"), JSON, None, Bytes::new())
        .await
        .expect_err("a body past the limit is refused");

    assert_eq!(error, OutboundError::ResponseTooLarge { limit: 1_024 });
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_rather_than_hanging() {
    // Port 1 on loopback: nothing listens, so this is a refused connection
    // rather than a timeout, which is the distinction callers act on.
    let error = client(ClientConfig::default())
        .post(
            &Endpoint::parse("http://127.0.0.1:1/admit").expect("a loopback URL is accepted"),
            JSON,
            None,
            Bytes::new(),
        )
        .await
        .expect_err("nothing is listening");

    assert!(matches!(error, OutboundError::Unreachable(_)));
}

#[test]
fn cleartext_endpoints_are_accepted_and_reported_as_unencrypted() {
    // `http://auth-sidecar:8081` is the ordinary in-cluster deployment, and
    // whatever protects that hop — a mesh, a private network — is not visible
    // from the URL. So the scheme is reported, not judged.
    for cleartext in [
        "http://127.0.0.1:8081/admit",
        "http://auth-sidecar:8081/admit",
        "http://auth.example.com/admit",
    ] {
        let endpoint = Endpoint::parse(cleartext).expect("{cleartext} is a usable endpoint");
        assert!(!endpoint.is_encrypted());
    }

    assert!(
        Endpoint::parse("https://auth.example.com/admit")
            .expect("an https URL is accepted")
            .is_encrypted()
    );
}

#[test]
fn an_endpoint_must_name_a_host_over_a_supported_scheme() {
    assert!(matches!(
        Endpoint::parse("ftp://example.com/admit"),
        Err(EndpointError::Scheme(_))
    ));
    assert!(matches!(
        Endpoint::parse("/admit"),
        Err(EndpointError::Scheme(_))
    ));
    assert!(matches!(
        Endpoint::parse("https://"),
        Err(EndpointError::Malformed(_) | EndpointError::NoHost(_))
    ));
}

#[tokio::test]
async fn a_retry_after_in_seconds_is_read_and_capped() {
    let address = start().await;

    let response = client(ClientConfig::default())
        .post(&endpoint(address, "/busy"), JSON, None, Bytes::new())
        .await
        .expect("the endpoint answers");

    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.retry_after(Duration::from_secs(30)),
        Some(Duration::from_secs(5))
    );
    assert_eq!(
        response.retry_after(Duration::from_secs(2)),
        Some(Duration::from_secs(2)),
        "the endpoint asking to be left alone is also the one misbehaving, so \
         the caller's cap still applies"
    );
}

#[tokio::test]
async fn an_absent_or_unparsable_retry_after_is_simply_absent() {
    let address = start().await;
    let client = client(ClientConfig::default());

    let dated = client
        .post(&endpoint(address, "/dated"), JSON, None, Bytes::new())
        .await
        .expect("the endpoint answers");
    let plain = client
        .post(&endpoint(address, "/teapot"), JSON, None, Bytes::new())
        .await
        .expect("the endpoint answers");

    assert_eq!(
        dated.retry_after(Duration::from_secs(30)),
        None,
        "the HTTP-date form falls back to the caller's own backoff"
    );
    assert_eq!(plain.retry_after(Duration::from_secs(30)), None);
}

#[test]
fn a_token_with_control_characters_is_refused() {
    // No `PartialEq` on the token itself: an equality impl on a secret is how
    // a non-constant-time comparison gets written later.
    assert!(BearerToken::new("line\nbreak").is_err());
    assert!(BearerToken::new("ordinary-token").is_ok());
}

#[test]
fn a_token_never_prints_itself() {
    let token = BearerToken::new("sidecar-secret").expect("a printable token is valid");

    assert_eq!(format!("{token:?}"), "BearerToken([REDACTED])");
}

#[tokio::test]
async fn lazy_pool_consumers_keep_independent_response_limits()
-> Result<(), Box<dyn std::error::Error>> {
    let address = start().await;
    let mut pool = super::LazyHttpClient::default();
    assert!(pool.0.is_none());
    let small = pool.with_limits(Duration::from_secs(2), 1024)?;
    let large = pool.with_limits(Duration::from_secs(2), 512 * 1024)?;
    let endpoint = endpoint(address, "/enormous");
    assert!(matches!(
        small.post(&endpoint, JSON, None, Bytes::new()).await,
        Err(OutboundError::ResponseTooLarge { limit: 1024 })
    ));
    assert_eq!(
        large
            .post(&endpoint, JSON, None, Bytes::new())
            .await?
            .body
            .len(),
        256 * 1024
    );
    // Building another consumer cannot overwrite the first consumer's ceiling.
    assert!(matches!(
        small.post(&endpoint, JSON, None, Bytes::new()).await,
        Err(OutboundError::ResponseTooLarge { limit: 1024 })
    ));
    Ok(())
}
