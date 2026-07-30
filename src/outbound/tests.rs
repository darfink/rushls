//! Driven against a real listener, for the same reason the HTTP server is:
//! deadlines, body limits, and header handling only exist once bytes are
//! actually on a socket.

use std::{net::SocketAddr, time::Duration};

use axum::{Router, extract::Request, response::IntoResponse, routing::post};
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

/// Starts a server on an ephemeral loopback port and returns its address.
async fn start() -> SocketAddr {
    let router = Router::new()
        .route("/echo", post(echo))
        .route("/slow", post(slow))
        .route("/enormous", post(enormous))
        .route("/teapot", post(teapot));
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
fn cleartext_is_confined_to_loopback() {
    for accepted in [
        "http://127.0.0.1:8081/admit",
        "http://localhost:8081/admit",
        "http://[::1]:8081/admit",
        "https://auth.example.com/admit",
    ] {
        assert!(
            Endpoint::parse(accepted).is_ok(),
            "{accepted} should be accepted"
        );
    }

    assert_eq!(
        Endpoint::parse("http://auth.example.com/admit"),
        Err(EndpointError::Cleartext(
            "http://auth.example.com/admit".into()
        )),
        "a bearer token must not reach a remote host in the clear"
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
