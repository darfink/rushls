//! Delivery mechanics, exercised through a vocabulary that belongs to nobody.
//!
//! The event type here is deliberately not either application's. If these tests
//! can be written against a toy `Occurrence`, the crate is reusable; if they
//! start needing streams or sessions, something application-shaped has leaked
//! back in.

use std::{
    collections::BTreeSet,
    error::Error,
    future::pending,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use parking_lot::Mutex;
use tokio::sync::watch;

use cc_outbound::{ClientConfig, Endpoint, HttpClient};

use crate::{
    HookConfig, HookObserver, HooksConfig, Loss, Occurrence, Queue, Renderer, Subject, build,
};

/// A subject with no meaning beyond being a hash key and a string.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Thing(String);

impl Thing {
    fn new(name: &str) -> Self {
        Self(name.to_owned())
    }
}

impl Subject for Thing {
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, derive_more::Display)]
enum Happening {
    #[display("thing.began")]
    Began,
    #[display("thing.finished")]
    Finished,
}

#[derive(Clone, Debug)]
struct Happened {
    thing: Thing,
    what: Happening,
    detail: u64,
}

impl Occurrence for Happened {
    type Subject = Thing;
    type Kind = Happening;

    const TYPE_PREFIX: &'static str = "example";

    fn kind(&self) -> Self::Kind {
        self.what
    }

    fn subject(&self) -> Self::Subject {
        self.thing.clone()
    }

    fn data(&self) -> serde_json::Value {
        serde_json::json!({ "thing": self.thing.0, "detail": self.detail })
    }
}

fn began(thing: &str, detail: u64) -> Happened {
    Happened {
        thing: Thing::new(thing),
        what: Happening::Began,
        detail,
    }
}

fn envelope(thing: &str) -> crate::Envelope<Thing, Happening> {
    Renderer::new("urn:example:node:test", 1)
        .render(&began(thing, 1))
        .expect("an event renders")
}

/// Records what the endpoint received, and decides what it answers with.
#[derive(Clone, Default)]
struct Recorder {
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    wire: Arc<Mutex<Vec<(http::HeaderMap, Bytes)>>>,
    /// Answered until `failures` requests have been served.
    failing_status: Arc<AtomicU16>,
    failures: Arc<AtomicUsize>,
}

impl Recorder {
    fn bodies(&self) -> Vec<serde_json::Value> {
        self.received.lock().clone()
    }

    fn types(&self) -> Vec<String> {
        self.bodies()
            .iter()
            .map(|body| body["type"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

async fn receive(
    State(recorder): State<Recorder>,
    headers: http::HeaderMap,
    body: Bytes,
) -> StatusCode {
    recorder.wire.lock().push((headers, body.clone()));
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("a hook body is always JSON");
    recorder.received.lock().push(body);
    if recorder.failures.load(Ordering::Relaxed) > 0 {
        recorder.failures.fetch_sub(1, Ordering::Relaxed);
        return StatusCode::from_u16(recorder.failing_status.load(Ordering::Relaxed))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    }
    StatusCode::NO_CONTENT
}

/// Accepts the request, records it, and never answers.
async fn receive_without_answer(State(recorder): State<Recorder>, body: Bytes) -> StatusCode {
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("a hook body is always JSON");
    recorder.received.lock().push(body);
    pending::<()>().await;
    unreachable!("the request never completes")
}

async fn start(recorder: Recorder) -> SocketAddr {
    serve(
        Router::new()
            .route("/events", post(receive))
            .with_state(recorder),
    )
    .await
}

async fn start_hanging(recorder: Recorder) -> SocketAddr {
    serve(
        Router::new()
            .route("/events", post(receive_without_answer))
            .with_state(recorder),
    )
    .await
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

fn hook(address: SocketAddr, events: &[Happening]) -> HookConfig<Happening> {
    HookConfig {
        name: "test".into(),
        endpoint: Endpoint::parse(&format!("http://{address}/events"))
            .expect("a loopback URL is accepted"),
        events: events.iter().copied().collect(),
        queue_capacity: 64,
        maximum_in_flight: 4,
        maximum_attempts: 3,
        bearer: None,
        signing_secret: None,
        client: None,
    }
}

fn config(hooks: Vec<HookConfig<Happening>>) -> HooksConfig<Happening> {
    HooksConfig {
        hooks,
        drain_timeout: Duration::from_secs(2),
        ..HooksConfig::new("urn:example:node:test")
    }
}

/// Collects what the crate reported about its own delivery.
#[derive(Clone, Default)]
struct Reported {
    dropped: Arc<Mutex<Vec<(String, Happening, Loss)>>>,
    outcomes_unknown: Arc<Mutex<Vec<(String, usize)>>>,
}

impl HookObserver<Happening> for Reported {
    fn event_dropped(&self, hook: &str, _event: &str, kind: Happening, loss: Loss, _detail: &str) {
        self.dropped.lock().push((hook.to_owned(), kind, loss));
    }

    fn delivery_outcomes_unknown(&self, hook: &str, count: usize) {
        self.outcomes_unknown.lock().push((hook.to_owned(), count));
    }
}

/// Runs the dispatchers until every expected request has arrived, then stops.
async fn deliver(
    hooks: HookConfig<Happening>,
    events: &[Happened],
    expected: usize,
    recorder: &Recorder,
) {
    deliver_reporting(hooks, events, expected, recorder, Reported::default()).await;
}

async fn deliver_reporting(
    hooks: HookConfig<Happening>,
    events: &[Happened],
    expected: usize,
    recorder: &Recorder,
    reported: Reported,
) {
    let client = HttpClient::new(ClientConfig {
        request_timeout: Duration::from_secs(2),
        ..ClientConfig::default()
    })
    .expect("a client builds");
    let (producer, dispatchers) =
        build(config(vec![hooks]), client, Arc::new(reported)).expect("valid fixture");
    let (stop_tx, stop_rx) = watch::channel(false);
    let running = tokio::spawn(dispatchers.run(stop_rx));

    for event in events {
        producer.deliver(event);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while recorder.bodies().len() < expected && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

#[tokio::test]
async fn an_event_arrives_as_a_cloudevent_naming_its_subject() {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;

    deliver(
        hook(address, &[Happening::Began]),
        &[began("workshop/lathe", 7)],
        1,
        &recorder,
    )
    .await;

    let body = recorder
        .bodies()
        .first()
        .cloned()
        .expect("one event arrived");
    assert_eq!(body["specversion"], "1.0");
    assert_eq!(
        body["type"], "example.thing.began.v1",
        "the type is the application's prefix, its own kind, and the schema version"
    );
    assert_eq!(body["source"], "urn:example:node:test");
    assert_eq!(body["subject"], "workshop/lathe");
    assert_eq!(body["datacontenttype"], "application/json");
    assert!(body["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(body["time"].as_str().is_some_and(|time| time.contains('T')));
    assert_eq!(
        body["data"],
        serde_json::json!({ "thing": "workshop/lathe", "detail": 7 }),
        "data is the application's to shape; this crate never interprets it"
    );
}

#[tokio::test]
async fn a_retry_repeats_the_same_event_id() {
    let recorder = Recorder::default();
    recorder
        .failing_status
        .store(StatusCode::SERVICE_UNAVAILABLE.as_u16(), Ordering::Relaxed);
    recorder.failures.store(1, Ordering::Relaxed);
    let address = start(recorder.clone()).await;

    deliver(
        hook(address, &[Happening::Began]),
        &[began("workshop/lathe", 1)],
        2,
        &recorder,
    )
    .await;

    let bodies = recorder.bodies();
    assert_eq!(bodies.len(), 2, "the 503 was retried");
    assert_eq!(
        bodies[0]["id"], bodies[1]["id"],
        "a redelivery is the same occurrence, so a consumer can deduplicate it"
    );
    assert_eq!(bodies[0]["time"], bodies[1]["time"]);
}

#[test]
fn producer_ingress_is_bounded_and_never_waits_for_the_dispatcher() -> Result<(), Box<dyn Error>> {
    let mut hook = hook("127.0.0.1:1".parse()?, &[Happening::Began]);
    hook.queue_capacity = 1;
    let client = HttpClient::new(ClientConfig::default())?;
    let (hooks, _dispatchers) = build(
        config(vec![hook]),
        client,
        Arc::new(crate::IgnoreHookEvents),
    )?;

    hooks.deliver(&began("workshop/first", 1));
    hooks.deliver(&began("workshop/second", 2));

    let snapshot = hooks.snapshots()[0].1;
    assert_eq!(snapshot.ingress_depth, 1);
    assert_eq!(snapshot.ingress, 1);
    assert_eq!(snapshot.queue_depth, 0);
    Ok(())
}

#[tokio::test]
async fn shutdown_distinguishes_never_sent_from_unknown_outcomes() -> Result<(), Box<dyn Error>> {
    let recorder = Recorder::default();
    let address = start_hanging(recorder.clone()).await;
    let mut hook = hook(address, &[Happening::Began]);
    hook.maximum_in_flight = 1;
    hook.maximum_attempts = 1;
    let reported = Reported::default();
    let client = HttpClient::new(ClientConfig {
        request_timeout: Duration::from_secs(5),
        ..ClientConfig::default()
    })?;
    let (hooks, dispatchers) = build(
        HooksConfig {
            drain_timeout: Duration::from_millis(20),
            ..config(vec![hook])
        },
        client,
        Arc::new(reported.clone()),
    )?;
    let (stop, stopped) = watch::channel(false);
    let running = tokio::spawn(dispatchers.run(stopped));

    hooks.deliver(&began("workshop/lathe", 1));
    hooks.deliver(&began("workshop/lathe", 2));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while recorder.bodies().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(recorder.bodies().len(), 1, "one request is in flight");

    stop.send(true)?;
    tokio::time::timeout(Duration::from_secs(2), running).await??;

    let snapshot = hooks.snapshots()[0].1;
    assert_eq!(snapshot.shutdown, 1, "the second event was never sent");
    assert_eq!(
        snapshot.outcome_unknown_shutdown, 1,
        "the first request may have reached the endpoint"
    );
    assert_eq!(
        reported.outcomes_unknown.lock().as_slice(),
        [("test".to_owned(), 1)]
    );
    Ok(())
}

#[tokio::test]
async fn a_client_rejection_is_not_retried() {
    let recorder = Recorder::default();
    recorder
        .failing_status
        .store(StatusCode::BAD_REQUEST.as_u16(), Ordering::Relaxed);
    recorder.failures.store(10, Ordering::Relaxed);
    let address = start(recorder.clone()).await;
    let reported = Reported::default();

    deliver_reporting(
        hook(address, &[Happening::Began]),
        &[began("workshop/lathe", 1)],
        1,
        &recorder,
        reported.clone(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        recorder.bodies().len(),
        1,
        "a 400 means the consumer rejected the payload; retrying would only hold +     up everything queued behind it for this subject"
    );
    // Reported with the application's own kind rather than a rendered string, so
    // whatever the application logs or counts stays typed.
    assert_eq!(
        reported.dropped.lock().as_slice(),
        [("test".to_owned(), Happening::Began, Loss::Rejected)]
    );
}

#[tokio::test]
async fn only_subscribed_events_are_delivered() {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;
    let finished = Happened {
        thing: Thing::new("workshop/lathe"),
        what: Happening::Finished,
        detail: 2,
    };

    deliver(
        hook(address, &[Happening::Finished]),
        &[began("workshop/lathe", 1), finished],
        1,
        &recorder,
    )
    .await;

    assert_eq!(recorder.types(), ["example.thing.finished.v1"]);
}

#[test]
fn one_subject_has_one_request_in_flight_at_a_time() {
    let mut queue = Queue::new(16);
    let lathe = Thing::new("workshop/lathe");
    queue.push(envelope("workshop/lathe"));
    queue.push(envelope("workshop/press"));
    queue.push(envelope("workshop/lathe"));

    let first = queue.take_ready().expect("a subject is ready");
    let second = queue.take_ready().expect("another subject is ready");

    assert_eq!(first.subject, lathe);
    assert_eq!(
        second.subject,
        Thing::new("workshop/press"),
        "a second subject goes concurrently; only one *per subject* is held back"
    );
    assert!(
        queue.take_ready().is_none(),
        "the lathe's next event waits for the one ahead of it: out-of-order +     delivery is what would tell a consumer something had finished early"
    );

    queue.finish(&lathe);
    assert_eq!(
        queue.take_ready().map(|envelope| envelope.subject),
        Some(lathe)
    );
}

#[test]
fn overflow_drops_the_oldest_so_survivors_stay_in_order() {
    let mut queue = Queue::new(2);

    assert!(!queue.push(envelope("workshop/a")));
    assert!(!queue.push(envelope("workshop/b")));
    let evicted = queue.push(envelope("workshop/c"));

    assert!(evicted, "the third event put the queue over capacity");
    assert_eq!(queue.queued(), 2);
    let remaining: Vec<Thing> = std::iter::from_fn(|| queue.take_ready())
        .map(|envelope| envelope.subject)
        .collect();
    assert_eq!(
        remaining,
        [Thing::new("workshop/b"), Thing::new("workshop/c")],
        "the newest state is what a consumer needs; dropping it would freeze +     their view at whatever was oldest"
    );
}

#[test]
fn losses_are_counted_apart_because_they_mean_different_things() {
    let names: BTreeSet<String> = [
        Loss::Ingress,
        Loss::Overflow,
        Loss::Rejected,
        Loss::Exhausted,
        Loss::Shutdown,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();

    assert_eq!(names.len(), 5);
}

#[tokio::test]
async fn signed_retries_cover_the_exact_wire_body_and_stable_event_id() -> Result<(), Box<dyn Error>>
{
    use base64::{Engine, engine::general_purpose::STANDARD};
    let recorder = Recorder::default();
    recorder.failures.store(1, Ordering::Relaxed);
    recorder.failing_status.store(503, Ordering::Relaxed);
    let address = start(recorder.clone()).await;
    let mut destination = hook(address, &[Happening::Began]);
    destination.signing_secret = Some(crate::SigningSecret::parse(&format!(
        "whsec_{}",
        STANDARD.encode([7; 32])
    ))?);
    destination.bearer = Some(cc_outbound::BearerToken::new("shared-token")?);
    deliver(destination, &[began("camera", 1)], 2, &recorder).await;
    let wire = recorder.wire.lock();
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[0].1, wire[1].1);
    assert_eq!(wire[0].0["webhook-id"], wire[1].0["webhook-id"]);
    for (headers, body) in wire.iter() {
        let parsed: serde_json::Value = serde_json::from_slice(body)?;
        assert_eq!(
            headers["webhook-id"].to_str()?,
            parsed["id"].as_str().unwrap()
        );
        assert_eq!(headers["authorization"], "Bearer shared-token");
        assert_eq!(headers["content-type"], crate::CONTENT_TYPE);
        let timestamp: i64 = headers["webhook-timestamp"].to_str()?.parse()?;
        assert!((time::OffsetDateTime::now_utc().unix_timestamp() - timestamp).abs() < 10);
        let mut signed = format!("{}.{}.", headers["webhook-id"].to_str()?, timestamp).into_bytes();
        signed.extend_from_slice(body);
        let signature = STANDARD.decode(
            headers["webhook-signature"]
                .to_str()?
                .strip_prefix("v1,")
                .unwrap(),
        )?;
        assert!(
            ring::hmac::verify(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7; 32]),
                &signed,
                &signature
            )
            .is_ok()
        );
        signed.push(b' ');
        assert!(
            ring::hmac::verify(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7; 32]),
                &signed,
                &signature
            )
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn invalid_destinations_fail_before_channels_are_created() -> Result<(), Box<dyn Error>> {
    let valid = hook("127.0.0.1:1".parse()?, &[Happening::Began]);
    let client = HttpClient::new(ClientConfig::default())?;
    for case in 0..5 {
        let mut invalid = valid.clone();
        match case {
            0 => invalid.queue_capacity = 0,
            1 => invalid.maximum_in_flight = 0,
            2 => invalid.maximum_attempts = 0,
            3 => invalid.events.clear(),
            _ => invalid.queue_capacity = usize::MAX,
        }
        let result = build::<Happened>(
            config(vec![valid.clone(), invalid]),
            client.clone(),
            Arc::new(crate::IgnoreHookEvents),
        );
        assert!(
            result.is_err(),
            "invalid configuration {case} must not panic or start"
        );
    }
    let result = build::<Happened>(
        config(vec![valid.clone(), valid]),
        client,
        Arc::new(crate::IgnoreHookEvents),
    );
    assert_eq!(
        result.err().map(|error| error.reason),
        Some("duplicate destination name")
    );
    Ok(())
}
