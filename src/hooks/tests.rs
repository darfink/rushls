//! What this node promises consumers, as distinct from how delivery works.
//!
//! Ordering, overflow, retry, and the drain are `cc-hooks`' contract and are
//! tested there against a vocabulary belonging to no application. What is left
//! here is the part a fork of this node would have to keep: the event names on
//! the wire, the shape of each payload, and the fact that projecting a real
//! publication produces them in the right order.

use std::{net::SocketAddr, sync::Arc, time::Duration};

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

use super::{HookConfig, HookObserver, HooksConfig, build};

/// Records what the endpoint received.
#[derive(Clone, Default)]
struct Recorder {
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Answered with this status until `failures` requests have been served.
    failing_status: Arc<std::sync::atomic::AtomicU16>,
    failures: Arc<std::sync::atomic::AtomicUsize>,
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
    use std::sync::atomic::Ordering;

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

async fn start(recorder: Recorder) -> SocketAddr {
    let app = Router::new()
        .route("/events", post(receive))
        .with_state(recorder);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
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
        signing_secret: None,
        // Nothing to authenticate over loopback, so the shared pool serves.
        client: None,
    }
}

fn started(stream: &str, session: u64) -> Event {
    Event::SessionStarted(SessionStarted {
        stream: StreamId::new(stream),
        session: SessionId(session.try_into().expect("a nonzero session id")),
        principal: "studio-camera".into(),
        publisher: crate::domain::fixtures::publisher(),
    })
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
            hooks: vec![hooks],
            drain_timeout: Duration::from_secs(2),
            ..HooksConfig::new("urn:rushls:node:test")
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
async fn a_node_observer_turns_a_publication_into_deliveries_and_still_reports_it() {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;
    let client = HttpClient::new(ClientConfig::default()).expect("a client builds");
    let seen = NodeLog::default();
    let (hooks, dispatchers) = build(
        HooksConfig {
            hooks: vec![hook(address, &lifecycle::Kind::ALL)],
            drain_timeout: Duration::from_secs(2),
            ..HooksConfig::new("urn:rushls:node:test")
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
            publisher: crate::domain::fixtures::publisher(),
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
        "both lifetimes arrive interleaved on one ordered stream: the +         publisher stops before the stream does"
    );
    let bodies = recorder.bodies();
    for body in [&bodies[0], &bodies[2]] {
        assert_eq!(body["data"]["protocol"], "rtmp");
        assert_eq!(body["data"]["client"]["remote_address"], "127.0.0.1:1935");
        assert_eq!(body["data"]["resource"]["name"], "presented-key");
        assert!(body["data"].get("credential").is_none());
    }
    assert_eq!(
        seen.sessions.lock().len(),
        3,
        "the observer decorates rather than replaces: a node keeps the +         reporting it already had"
    );
}

#[tokio::test]
async fn a_dropped_event_is_reported_through_the_node_observer() {
    use std::sync::atomic::Ordering;

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
    // embedder that redirects it redirects this too. This is also what keeps
    // the shared crate free of any opinion about how a node reports.
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
        publisher: crate::domain::fixtures::publisher(),
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
            publisher: crate::domain::fixtures::publisher(),
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
        "a session's events describe the publisher only; what viewers can +         reach is the store's to report"
    );
}
