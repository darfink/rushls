//! Driven against a real sidecar, because the failure modes worth testing —
//! a timeout, a 500, a body that is not a decision — only exist over a socket.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use parking_lot::Mutex;

use crate::{
    admission::{
        AdmissionError, Authenticator, IngestProtocol, StreamPolicy, TakeoverPolicy, fixtures,
    },
    domain::StreamId,
    outbound::{ClientConfig, Endpoint, HttpClient},
};

use super::{HttpAuthConfig, HttpAuthenticator};

/// Answers with whatever the test told it to, and keeps what it was asked.
struct Sidecar {
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    status: StatusCode,
    body: &'static str,
    delay: Duration,
}

impl Sidecar {
    fn answering(body: &'static str) -> Self {
        Self {
            received: Arc::default(),
            status: StatusCode::OK,
            body,
            delay: Duration::ZERO,
        }
    }

    fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    fn slow(mut self) -> Self {
        self.delay = Duration::from_secs(30);
        self
    }

    fn asked(&self) -> Option<serde_json::Value> {
        self.received.lock().first().cloned()
    }
}

async fn admit(State(sidecar): State<Sidecar>, body: Bytes) -> (StatusCode, &'static str) {
    sidecar
        .received
        .lock()
        .push(serde_json::from_slice(&body).expect("the request is JSON"));
    if !sidecar.delay.is_zero() {
        tokio::time::sleep(sidecar.delay).await;
    }
    (sidecar.status, sidecar.body)
}

async fn start(sidecar: Sidecar) -> SocketAddr {
    let router = Router::new()
        .route("/admit", post(admit))
        .with_state(sidecar);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port is available");
    let address = listener.local_addr().expect("the listener is bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    address
}

fn authenticator(address: SocketAddr, config: HttpAuthConfig) -> HttpAuthenticator {
    HttpAuthenticator::new(
        HttpAuthConfig {
            endpoint: Endpoint::parse(&format!("http://{address}/admit"))
                .expect("a loopback URL is accepted"),
            ..config
        },
        HttpClient::new(ClientConfig {
            request_timeout: Duration::from_millis(500),
            ..ClientConfig::default()
        })
        .expect("a client builds"),
    )
}

#[tokio::test]
async fn an_allowing_service_decides_identity_and_selects_a_local_policy() {
    let mut policies = HttpAuthConfig::default().policies;
    policies.insert("protected".into(), {
        let mut policy = StreamPolicy::permissive();
        policy.takeovers = TakeoverPolicy::Deny;
        policy
    });
    let sidecar = Sidecar::answering(
        r#"{"decision":"allow","stream_id":"events/main","principal":"account-42","policy":"protected"}"#,
    );
    let address = start(sidecar.clone()).await;

    let grant = authenticator(
        address,
        HttpAuthConfig {
            policies,
            ..HttpAuthConfig::default()
        },
    )
    .authenticate(&fixtures::publish_request("a-key"))
    .await
    .expect("the service allows the publisher");

    assert_eq!(grant.stream_id, StreamId::new("events/main"));
    assert_eq!(grant.principal.0, "account-42");
    assert_eq!(
        grant.policy.takeovers,
        TakeoverPolicy::Deny,
        "the response selects a policy by name; the limits stay this node's"
    );
}

#[tokio::test]
async fn the_request_carries_the_publisher_without_mangling_its_credential() {
    let sidecar =
        Sidecar::answering(r#"{"decision":"allow","stream_id":"live/x","principal":"p"}"#);
    let address = start(sidecar.clone()).await;

    authenticator(address, HttpAuthConfig::default())
        .authenticate(&fixtures::publish_request("a-key"))
        .await
        .expect("the service allows the publisher");

    let asked = sidecar.asked().expect("the sidecar was asked");
    assert_eq!(asked["version"], 1);
    assert_eq!(asked["protocol"], "rtmp");
    assert_eq!(asked["resource"]["namespace"], "live");
    assert_eq!(asked["resource"]["name"], "presented-key");
    assert_eq!(asked["credential"]["encoding"], "base64");
    assert_eq!(
        asked["credential"]["value"], "YS1rZXk=",
        "a credential is bytes, and need not be text at all"
    );
    assert!(asked["client"]["remote_address"].is_string());
    assert!(
        asked["request_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "a service that wants idempotence needs a key to use"
    );
}

#[tokio::test]
async fn a_moq_publisher_is_named_as_moq_on_the_wire() {
    let sidecar =
        Sidecar::answering(r#"{"decision":"allow","stream_id":"live/x","principal":"p"}"#);
    let address = start(sidecar.clone()).await;

    let mut request = fixtures::publish_request("a-key");
    request.protocol = IngestProtocol::Moq;
    authenticator(address, HttpAuthConfig::default())
        .authenticate(&request)
        .await
        .expect("the service allows the publisher");

    let asked = sidecar.asked().expect("the sidecar was asked");
    assert_eq!(asked["protocol"], "moq");
    assert_eq!(asked["version"], 1);
}

#[tokio::test]
async fn a_denying_service_refuses_the_publisher() {
    let sidecar = Sidecar::answering(r#"{"decision":"deny","reason":"subscription_inactive"}"#);
    let address = start(sidecar).await;

    let error = authenticator(address, HttpAuthConfig::default())
        .authenticate(&fixtures::publish_request("a-key"))
        .await
        .expect_err("the service denies the publisher");

    assert_eq!(
        error,
        AdmissionError::InvalidCredential,
        "a deny is the service working, not the service failing"
    );
}

#[tokio::test]
async fn every_way_of_not_getting_an_answer_denies() {
    let unparsable = Sidecar::answering(r#"{"verdict":"maybe"}"#);
    let erroring = Sidecar::answering("upstream is down").with_status(StatusCode::BAD_GATEWAY);
    let unknown_policy = Sidecar::answering(
        r#"{"decision":"allow","stream_id":"live/x","principal":"p","policy":"nonexistent"}"#,
    );
    let nameless = Sidecar::answering(r#"{"decision":"allow","stream_id":"  ","principal":"p"}"#);
    let silent = Sidecar::answering("never arrives").slow();

    for sidecar in [unparsable, erroring, unknown_policy, nameless, silent] {
        let address = start(sidecar).await;
        let error = authenticator(address, HttpAuthConfig::default())
            .authenticate(&fixtures::publish_request("a-key"))
            .await
            .expect_err("nothing decided, so nothing is admitted");

        assert!(
            matches!(error, AdmissionError::Service(_)),
            "an authority that cannot answer must not let a publisher through: {error:?}"
        );
    }
}

#[tokio::test]
async fn an_unreachable_service_denies_rather_than_hanging() {
    let error = HttpAuthenticator::new(
        HttpAuthConfig {
            endpoint: Endpoint::parse("http://127.0.0.1:1/admit").expect("a loopback URL"),
            ..HttpAuthConfig::default()
        },
        HttpClient::new(ClientConfig::default()).expect("a client builds"),
    )
    .authenticate(&fixtures::publish_request("a-key"))
    .await
    .expect_err("nothing is listening");

    assert!(matches!(error, AdmissionError::Service(_)));
}

#[tokio::test]
async fn the_service_is_asked_once_and_only_once() {
    let sidecar = Sidecar::answering("boom").with_status(StatusCode::INTERNAL_SERVER_ERROR);
    let address = start(sidecar.clone()).await;

    let _ = authenticator(address, HttpAuthConfig::default())
        .authenticate(&fixtures::publish_request("a-key"))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        sidecar.received.lock().len(),
        1,
        "admission sits on the publisher's deadline and the service may have \
         side effects, so a failure is final rather than retried"
    );
}

impl Clone for Sidecar {
    fn clone(&self) -> Self {
        Self {
            received: Arc::clone(&self.received),
            status: self.status,
            body: self.body,
            delay: self.delay,
        }
    }
}
