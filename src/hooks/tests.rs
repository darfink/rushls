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

use crate::{
    domain::{SessionId, StreamId},
    observe::{
        EventObserver, Events, NodeEvent, SessionEnd, SessionEvent, StreamEvent,
        lifecycle::{self, Event, Projector, SessionStarted},
    },
    outbound::{ClientConfig, Endpoint, HttpClient},
};

use super::{
    HookConfig, HookObserver, HooksConfig, Loss, Queue, Renderer, build, envelope::Envelope,
};

/// Records what the endpoint received, and decides what it answers with.
#[derive(Clone, Default)]
struct Recorder {
    received: Arc<Mutex<Vec<serde_json::Value>>>,
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

async fn receive(State(recorder): State<Recorder>, body: Bytes) -> StatusCode {
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

async fn receive_without_answer(State(recorder): State<Recorder>, body: Bytes) -> StatusCode {
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("a hook body is always JSON");
    recorder.received.lock().push(body);
    pending().await
}

async fn start(recorder: Recorder) -> SocketAddr {
    let router = Router::new()
        .route("/events", post(receive))
        .with_state(recorder);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port is available");
    let address = listener.local_addr().expect("the listener is bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    address
}

async fn start_hanging(recorder: Recorder) -> SocketAddr {
    let router = Router::new()
        .route("/events", post(receive_without_answer))
        .with_state(recorder);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port is available");
    let address = listener.local_addr().expect("the listener is bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    address
}

fn hook(address: SocketAddr, events: &[lifecycle::Kind]) -> HookConfig {
    HookConfig {
        name: "test".into(),
        endpoint: Endpoint::parse(&format!("http://{address}/events"))
            .expect("a loopback URL is accepted"),
        events: events.iter().copied().collect(),
        queue_capacity: 64,
        maximum_in_flight: 4,
        maximum_attempts: 3,
        bearer: None,
    }
}

fn started(stream: &str, session: u64) -> Event {
    Event::SessionStarted(SessionStarted {
        stream: StreamId::new(stream),
        session: SessionId(session.try_into().expect("a nonzero session id")),
        principal: "studio-camera".into(),
    })
}

fn envelope(stream: &str) -> Envelope {
    Renderer::new("urn:rushls:node:test", 1)
        .render(&started(stream, 1))
        .expect("an event renders")
}

/// Collects what the node reported about its own delivery.
#[derive(Clone, Default)]
struct NodeLog {
    node: Arc<Mutex<Vec<NodeEvent>>>,
    sessions: Arc<Mutex<Vec<SessionEvent>>>,
}

impl EventObserver for NodeLog {
    fn observe(&self, _session: SessionId, event: SessionEvent) {
        self.sessions.lock().push(event);
    }

    fn observe_node(&self, event: NodeEvent) {
        self.node.lock().push(event);
    }
}

/// Runs the dispatchers until every expected request has arrived, then stops.
async fn deliver(hooks: HookConfig, events: &[Event], expected: usize, recorder: &Recorder) {
    deliver_reporting(hooks, events, expected, recorder, Events::default()).await;
}

async fn deliver_reporting(
    hooks: HookConfig,
    events: &[Event],
    expected: usize,
    recorder: &Recorder,
    reported: Events,
) {
    let client = HttpClient::new(ClientConfig {
        request_timeout: Duration::from_secs(2),
        ..ClientConfig::default()
    })
    .expect("a client builds");
    let (producer, dispatchers) = build(
        HooksConfig {
            source: "urn:rushls:node:test".into(),
            hooks: vec![hooks],
            drain_timeout: Duration::from_secs(2),
            ..HooksConfig::default()
        },
        client,
        reported,
    );
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
async fn an_event_arrives_as_a_cloudevent_naming_its_stream() {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;

    deliver(
        hook(address, &[lifecycle::Kind::SessionStarted]),
        &[started("live/camera", 7)],
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
    assert_eq!(body["type"], "rushls.session.started.v1");
    assert_eq!(body["source"], "urn:rushls:node:test");
    assert_eq!(body["subject"], "live/camera");
    assert_eq!(body["datacontenttype"], "application/json");
    assert!(body["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(body["time"].as_str().is_some_and(|time| time.contains('T')));
    assert_eq!(body["data"]["stream_id"], "live/camera");
    assert_eq!(
        body["data"]["session_id"], "7",
        "a 64-bit id must survive a consumer that parses JSON numbers as doubles"
    );
    assert_eq!(body["data"]["principal"], "studio-camera");
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
        hook(address, &[lifecycle::Kind::SessionStarted]),
        &[started("live/camera", 1)],
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
    let mut config = hook("127.0.0.1:1".parse()?, &[lifecycle::Kind::SessionStarted]);
    config.queue_capacity = 1;
    let client = HttpClient::new(ClientConfig::default())?;
    let (hooks, _dispatchers) = build(
        HooksConfig {
            hooks: vec![config],
            ..HooksConfig::default()
        },
        client,
        Events::default(),
    );

    hooks.deliver(&started("live/first", 1));
    hooks.deliver(&started("live/second", 2));

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
    let mut config = hook(address, &[lifecycle::Kind::SessionStarted]);
    config.maximum_in_flight = 1;
    config.maximum_attempts = 1;
    let reported = NodeLog::default();
    let client = HttpClient::new(ClientConfig {
        request_timeout: Duration::from_secs(5),
        ..ClientConfig::default()
    })?;
    let (hooks, dispatchers) = build(
        HooksConfig {
            hooks: vec![config],
            drain_timeout: Duration::from_millis(20),
            ..HooksConfig::default()
        },
        client,
        Events::new(Arc::new(reported.clone())),
    );
    let (stop, stopped) = watch::channel(false);
    let running = tokio::spawn(dispatchers.run(stopped));

    hooks.deliver(&started("live/camera", 1));
    hooks.deliver(&started("live/camera", 2));
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
    assert!(reported.node.lock().iter().any(|event| matches!(
        event,
        NodeEvent::HookDeliveryOutcomesUnknown { count: 1, .. }
    )));
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

    deliver(
        hook(address, &[lifecycle::Kind::SessionStarted]),
        &[started("live/camera", 1)],
        1,
        &recorder,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        recorder.bodies().len(),
        1,
        "a 400 means the consumer rejected the payload; retrying would only \
         hold up everything queued behind it for this stream"
    );
}

#[tokio::test]
async fn a_node_observer_turns_a_publication_into_deliveries_and_still_reports_it() {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;
    let client = HttpClient::new(ClientConfig::default()).expect("a client builds");
    let seen = NodeLog::default();
    let (hooks, dispatchers) = build(
        HooksConfig {
            source: "urn:rushls:node:test".into(),
            hooks: vec![hook(address, &lifecycle::Kind::ALL)],
            drain_timeout: Duration::from_secs(2),
            ..HooksConfig::default()
        },
        client,
        Events::default(),
    );
    let observer = HookObserver::new(hooks, Arc::new(seen.clone()));
    let (stop, stopped) = watch::channel(false);
    let running = tokio::spawn(dispatchers.run(stopped));

    // What a real publication emits, in the order a session emits it.
    let session = SessionId(7.try_into().expect("a nonzero session id"));
    observer.observe(
        session,
        SessionEvent::Accepted {
            stream: StreamId::new("live/camera"),
            principal: "camera".into(),
        },
    );
    observer.observe(session, SessionEvent::Running);
    // The store's answer, not the pipeline's: what a viewer can fetch is what
    // "available" means, and it arrives on the stream-scoped path.
    observer.observe_stream(StreamId::new("live/camera"), StreamEvent::Available);
    observer.observe(
        session,
        SessionEvent::Ended {
            end: SessionEnd::Ended,
        },
    );
    observer.observe_stream(StreamId::new("live/camera"), StreamEvent::Retired);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while recorder.bodies().len() < 4 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;

    assert_eq!(
        recorder.types(),
        [
            "rushls.session.started.v1",
            "rushls.stream.available.v1",
            "rushls.session.ended.v1",
            "rushls.stream.unavailable.v1",
        ],
        "both lifetimes arrive interleaved on one ordered stream: the \
         publisher stops before the stream does"
    );
    assert_eq!(
        seen.sessions.lock().len(),
        3,
        "the observer decorates rather than replaces: a node keeps the \
         reporting it already had"
    );
}

#[tokio::test]
async fn a_dropped_event_is_reported_through_the_node_observer() {
    let recorder = Recorder::default();
    recorder
        .failing_status
        .store(StatusCode::BAD_REQUEST.as_u16(), Ordering::Relaxed);
    recorder.failures.store(10, Ordering::Relaxed);
    let address = start(recorder.clone()).await;
    let log = NodeLog::default();

    deliver_reporting(
        hook(address, &[lifecycle::Kind::SessionStarted]),
        &[started("live/camera", 1)],
        1,
        &recorder,
        Events::new(Arc::new(log.clone())),
    )
    .await;

    // Nothing here writes to stderr itself: the process owns its output, so an
    // embedder that redirects it redirects this too.
    let reported = log.node.lock().clone();
    let dropped = reported
        .iter()
        .find_map(|event| match event {
            NodeEvent::HookEventDropped {
                hook, kind, reason, ..
            } => Some((hook.to_string(), *kind, *reason)),
            _ => None,
        })
        .expect("a permanently rejected event is reported");

    assert_eq!(
        dropped,
        (
            "test".to_owned(),
            lifecycle::Kind::SessionStarted,
            "rejected"
        )
    );
}

#[tokio::test]
async fn only_subscribed_events_are_delivered() {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;
    let ended = Event::SessionEnded(lifecycle::SessionEnded {
        stream: StreamId::new("live/camera"),
        session: SessionId(nz::u64!(1)),
        principal: "studio-camera".into(),
        outcome: lifecycle::Outcome::Ended,
        duration: Duration::from_mins(3),
        was_available: true,
        diagnostic: None,
    });

    deliver(
        hook(address, &[lifecycle::Kind::SessionEnded]),
        &[started("live/camera", 1), ended],
        1,
        &recorder,
    )
    .await;

    assert_eq!(recorder.types(), ["rushls.session.ended.v1"]);
}

#[test]
fn one_stream_has_one_request_in_flight_at_a_time() {
    let mut queue = Queue::new(16);
    let camera = StreamId::new("live/camera");
    queue.push(envelope("live/camera"));
    queue.push(envelope("live/stage"));
    queue.push(envelope("live/camera"));

    let first = queue.take_ready().expect("a stream is ready");
    let second = queue.take_ready().expect("another stream is ready");

    assert_eq!(first.subject, camera);
    assert_eq!(
        second.subject,
        StreamId::new("live/stage"),
        "a second stream goes concurrently; only one *per stream* is held back"
    );
    assert!(
        queue.take_ready().is_none(),
        "the camera's next event waits for the one ahead of it: out-of-order \
         delivery is what would tell a consumer a live stream had ended"
    );

    queue.finish(&camera);
    assert_eq!(
        queue.take_ready().map(|envelope| envelope.subject),
        Some(camera)
    );
}

#[test]
fn overflow_drops_the_oldest_so_survivors_stay_in_order() {
    let mut queue = Queue::new(2);

    assert!(!queue.push(envelope("live/a")));
    assert!(!queue.push(envelope("live/b")));
    let evicted = queue.push(envelope("live/c"));

    assert!(evicted, "the third event put the queue over capacity");
    assert_eq!(queue.queued(), 2);
    let remaining: Vec<StreamId> = std::iter::from_fn(|| queue.take_ready())
        .map(|envelope| envelope.subject)
        .collect();
    assert_eq!(
        remaining,
        [StreamId::new("live/b"), StreamId::new("live/c")],
        "the newest state is what a consumer needs; dropping it would freeze \
         their view at whatever was oldest"
    );
}

#[test]
fn a_projected_session_reaches_the_hooks_it_subscribed_to() {
    // The two halves fit together: the projector decides what is public, and
    // hooks decide who hears it.
    let projector = Projector::new();
    let session = SessionId(nz::u64!(1));
    let mut kinds = Vec::new();

    for event in [
        SessionEvent::Accepted {
            stream: StreamId::new("live/camera"),
            principal: "studio-camera".into(),
        },
        SessionEvent::Running,
        SessionEvent::Draining,
        SessionEvent::Ended {
            end: SessionEnd::Ended,
        },
    ] {
        if let Some(public) = projector.project(session, &event) {
            kinds.push(public.kind());
        }
    }

    assert_eq!(
        kinds,
        [
            lifecycle::Kind::SessionStarted,
            lifecycle::Kind::SessionEnded,
        ],
        "a session's events describe the publisher only; what viewers can \
         reach is the store's to report"
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
