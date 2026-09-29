//! What this node promises consumers, as distinct from how delivery works.
//!
//! Ordering, overflow, retry, and the drain are `rushls-hooks`' contract and are
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
    )
    .expect("valid hook fixture");
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
    )
    .expect("valid hook fixture");
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
        bodies[1]["data"]["playlist_path"],
        "/live/camera/index.m3u8"
    );
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
        compensation: Vec::new(),
        timestamp_issue: None,
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

#[test]
fn timestamp_issue_hook_preserves_exact_ticks_and_is_absent_for_other_failures() {
    use crate::domain::{
        Codec, MediaKind, Timebase, TimestampField, TimestampIssue, TimestampIssueCode, TrackId,
    };
    use rushls_hooks::Occurrence;
    let mut ended = lifecycle::SessionEnded {
        compensation: Vec::new(),
        stream: StreamId::new("live/camera"),
        session: SessionId(nz::u64!(1)),
        principal: "publisher".into(),
        publisher: crate::domain::fixtures::publisher(),
        outcome: lifecycle::Outcome::Failed,
        duration: Duration::ZERO,
        was_available: false,
        diagnostic: Some("timing failed".into()),
        timestamp_issue: None,
    };
    assert!(
        Event::SessionEnded(ended.clone())
            .data()
            .get("timestamp_issue")
            .is_none()
    );
    ended.timestamp_issue = Some(Box::new(TimestampIssue {
        cadence: None,
        recovery_rejection: Some(crate::domain::RecoveryRejection::MaximumHole),
        code: TimestampIssueCode::AudioGap,
        track: TrackId(3),
        media_kind: MediaKind::Audio,
        codec: Codec::Opus,
        field: TimestampField::Pts,
        reference: i128::from(i64::MIN),
        actual: i128::from(i64::MAX),
        timebase: Timebase::new(nz::u32!(1), nz::u32!(48000)),
        tolerance_ticks: Some(48),
        maximum: None,
        missing_ticks: Some(u64::MAX),
    }));
    let data = Event::SessionEnded(ended).data();
    let issue = &data["timestamp_issue"];
    assert_eq!(issue["code"], "audio_gap");
    assert_eq!(issue["recovery_rejection"], "maximum_hole");
    assert_eq!(issue["reference"], i64::MIN.to_string());
    assert_eq!(issue["actual"], i64::MAX.to_string());
    assert_eq!(issue["missing_ticks"], u64::MAX.to_string());
    assert_eq!(issue["timebase"]["denominator"], 48000);
}

#[test]
fn recovery_hooks_preserve_exact_values_and_only_emit_episode_transitions()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::domain::{
        Codec, CompensationStatus, NormalizationNotice, RecoveryMethod, RecoveryTransition,
        Timebase, TrackId,
    };
    use rushls_hooks::Occurrence;
    let projector = Projector::new();
    let session = SessionId(nz::u64!(1));
    projector.project(
        session,
        &SessionEvent::Accepted {
            stream: StreamId::new("live/audio"),
            principal: "publisher".into(),
            publisher: crate::domain::fixtures::publisher(),
        },
    );
    let mut status = CompensationStatus {
        media_kind: crate::domain::MediaKind::Audio,
        cadence: None,
        track: TrackId(4),
        codec: Codec::Aac,
        method: RecoveryMethod::Gap,
        timebase: Timebase::new(nz::u32!(1), nz::u32!(48000)),
        missing_ticks: 1024,
        replacement_ticks: 1024,
        episode_holes: 1,
        episode_ticks: 1024,
        total_holes: 1,
        total_ticks: 9_007_199_254_740_993,
        degraded: true,
    };
    let degraded = projector
        .project(
            session,
            &SessionEvent::Compensation {
                notice: NormalizationNotice {
                    transition: RecoveryTransition::Degraded,
                    status: status.clone(),
                },
            },
        )
        .ok_or("degraded event")?;
    assert_eq!(degraded.kind().to_string(), "session.degraded");
    assert_eq!(
        degraded.data()["compensation"]["total_ticks"],
        "9007199254740993"
    );
    assert_eq!(
        degraded.data()["compensation"]["timebase"]["denominator"],
        48000
    );
    status.episode_holes = 2;
    assert!(
        projector
            .project(
                session,
                &SessionEvent::Compensation {
                    notice: NormalizationNotice {
                        transition: RecoveryTransition::Compensated,
                        status: status.clone()
                    }
                }
            )
            .is_none()
    );
    status.degraded = false;
    let recovered = projector
        .project(
            session,
            &SessionEvent::Compensation {
                notice: NormalizationNotice {
                    transition: RecoveryTransition::Recovered,
                    status,
                },
            },
        )
        .ok_or("recovered event")?;
    assert_eq!(recovered.kind().to_string(), "session.recovered");
    let ended = projector
        .project(
            session,
            &SessionEvent::Ended {
                end: SessionEnd::Ended,
            },
        )
        .ok_or("ended")?;
    assert_eq!(ended.data()["compensation"][0]["episode_holes"], "2");
    assert_eq!(ended.data()["compensation"][0]["degraded"], false);
    assert_eq!(
        "session.degraded".parse::<lifecycle::Kind>()?,
        lifecycle::Kind::SessionDegraded
    );
    assert_eq!(
        "session.recovered".parse::<lifecycle::Kind>()?,
        lifecycle::Kind::SessionRecovered
    );
    Ok(())
}

#[test]
fn video_compensation_hook_preserves_scope_and_exact_duration() {
    use crate::domain::{
        CadenceScope, CadenceSource, Codec, CompensationStatus, FrameRate, MediaKind,
        RecoveryMethod, Timebase, TrackId, VideoCadence,
    };
    let data = super::recovery_data(&CompensationStatus {
        media_kind: MediaKind::Video,
        cadence: Some(VideoCadence::Fixed {
            rate: FrameRate::new(nz::u32!(30000), nz::u32!(1001)),
            source: CadenceSource::H264Vui,
            scope: CadenceScope::ProgressiveFrames,
        }),
        track: TrackId(2),
        codec: Codec::H264,
        method: RecoveryMethod::Gap,
        timebase: Timebase::new(nz::u32!(1), nz::u32!(30000)),
        missing_ticks: 499,
        replacement_ticks: 0,
        episode_holes: 1,
        episode_ticks: 499,
        total_holes: 1,
        total_ticks: 499,
        degraded: true,
    });
    assert_eq!(data["media_kind"], "video");
    assert_eq!(data["method"], "gap");
    assert_eq!(data["missing_ticks"], "499");
    assert_eq!(data["cadence"]["source"], "h264_vui");
    assert_eq!(data["cadence"]["scope"], "progressive_frames");
    assert_eq!(data["cadence"]["interval"]["numerator"], 1001);
}

#[tokio::test]
async fn segment_ready_delivers_metadata_with_exact_timing()
-> Result<(), Box<dyn std::error::Error>> {
    let recorder = Recorder::default();
    let address = start(recorder.clone()).await;
    let event = Projector::new()
        .project_stream(
            StreamId::new("live/camera"),
            StreamEvent::SegmentReady(lifecycle::ReadySegment {
                rendition_id: 3,
                segment_id: u64::MAX,
                media_sequence: 42,
                publication: 2,
                path: "/live/camera/3/segment/18446744073709551615.m4s".into(),
                initialization_path: Some("/live/camera/3/init/1.mp4".into()),
                media_start: i64::MIN,
                duration: 90_000,
                timebase: crate::domain::Timebase::hz90k(),
                bytes: 123,
                independent: false,
                discontinuity: true,
            }),
        )
        .ok_or("segment projects without a session")?;
    assert_eq!(event.session(), None);
    deliver(
        hook(address, &[lifecycle::Kind::SegmentReady]),
        &[event],
        1,
        &recorder,
    )
    .await;
    let bodies = recorder.bodies();
    let body = bodies.first().ok_or("one hook arrived")?;
    assert_eq!(body["type"], "rushls.segment.ready.v1");
    assert_eq!(body["subject"], "live/camera");
    assert_eq!(
        body["data"],
        serde_json::json!({
            "stream_id": "live/camera", "rendition_id": 3,
            "segment_id": u64::MAX.to_string(), "media_sequence": "42", "publication": "2",
            "path": "/live/camera/3/segment/18446744073709551615.m4s",
            "initialization_path": "/live/camera/3/init/1.mp4",
            "media_start": i64::MIN.to_string(), "duration": "90000",
            "timebase": {"numerator": 1, "denominator": 90000},
            "bytes": "123", "independent": false, "discontinuity": true,
        })
    );
    Ok(())
}
