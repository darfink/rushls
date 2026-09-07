use std::{fmt::Write, net::SocketAddr, sync::Arc, time::Duration};

use cc_metrics::escape_label;

use crate::{
    delivery::hls::{RetentionDepth, StreamStore},
    domain::StreamId,
    hooks::{HOOK_SERIES, HookSnapshot, Hooks},
    observe::{
        HlsMeters, HlsSnapshot, MeterSnapshot, MetricKind, OriginMeters, OriginSnapshot,
        ProcessMeters, ProcessSnapshot, Series, counters::series,
    },
    server::http::playback::{PlaybackDenials, PlaybackMeters},
    session::{Registry, SessionSnapshot},
};

pub use cc_metrics::MetricsToken;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetricsConfig {
    /// Where metrics are served, or `None` to export nothing.
    ///
    /// Its own listener, defaulting to loopback: Prometheus series carry
    /// stream names, which on a public origin is the list of everything
    /// currently published and not something the viewer-facing port should
    /// offer. Setting this to the viewer address deliberately shares that
    /// port, which makes the sharing visible in the file rather than magic.
    ///
    /// Presence is the switch. An `enabled` flag beside an address that also
    /// accepted "off" was two disable switches meaning different things.
    pub listen: Option<SocketAddr>,
    /// When present, scrapes must authenticate with this bearer token.
    pub token: Option<MetricsToken>,
}

#[derive(Clone, Debug)]
pub struct MetricsSnapshot {
    pub process: ProcessSnapshot,
    pub origin: OriginSnapshot,
    pub hls: HlsSnapshot,
    pub active_sessions: usize,
    /// Streams with a publisher attached.
    pub published_streams: usize,
    /// Streams still fetchable but waiting for a publisher to return.
    pub idle_streams: usize,
    /// Delivery counters per configured hook.
    pub hooks: Vec<(Arc<str>, HookSnapshot)>,
    /// Payload bytes currently held for viewers, memory tier.
    pub retained_payload_bytes: usize,
    /// Payload bytes currently held for viewers on disk.
    pub retained_disk_bytes: usize,
    /// Sum of per-stream memory caps for streams this node currently retains.
    pub retention_memory_capacity: usize,
    /// Sum of per-stream disk caps; zero when the node is memory-only.
    pub retention_disk_capacity: usize,
    /// Spill jobs accepted and not yet finished.
    pub spill_pending: usize,
    /// Spill writes that failed; media stayed in memory.
    pub spills_failed: u64,
    /// Configured `retain`, identical for every stream on this node today.
    pub retention_requested: Duration,
    /// One reading per retained stream, including idle ones.
    pub retention: Vec<(StreamId, RetentionDepth)>,
    /// Viewer JWT denials, present only when playback authorization is on.
    pub playback: Option<PlaybackDenials>,
}

/// A configured HTTP exporter backed by the node's live metrics reader.
#[derive(Clone, Debug)]
pub struct MetricsEndpoint {
    reader: MetricsReader,
    token: Option<MetricsToken>,
}

impl MetricsEndpoint {
    pub fn new(reader: MetricsReader, token: Option<MetricsToken>) -> Self {
        Self { reader, token }
    }

    /// Checks a parsed bearer token without leaking timing information about
    /// matching prefixes of the configured secret.
    pub fn authorize(&self, presented: Option<&str>) -> bool {
        self.token.as_ref().is_none_or(|expected| {
            presented.is_some_and(|token| expected.matches(token.as_bytes()))
        })
    }

    pub fn render(&self) -> String {
        render(&self.reader.snapshot())
    }

    /// Live sessions and per-stream retention, including idle streams still
    /// within `retain`.
    ///
    /// Served at `/metrics/streams` so the scraper chooses the unbounded
    /// cardinality, rather than a node-side flag.
    pub fn render_streams(&self) -> String {
        render_labelled(
            &self.reader.sessions.snapshot(),
            &self.reader.retention_depths(),
        )
    }
}

/// Reads operational state for export.
///
/// Lives here rather than in `observe` so the observability layer stays at the
/// bottom of the dependency graph. Reading state is a composition concern; only
/// the process wiring should need to know about both meters and sessions.
#[derive(Clone, Debug)]
pub struct MetricsReader {
    meters: ProcessMeters,
    origin: OriginMeters,
    hls: HlsMeters,
    sessions: Registry,
    store: StreamStore,
    /// Absent unless hooks are configured, which is also when they have
    /// anything to report.
    hooks: Option<Hooks>,
    /// Absent unless `[auth.playback]` is configured.
    playback: Option<PlaybackMeters>,
}

impl MetricsReader {
    pub fn new(
        meters: ProcessMeters,
        origin: OriginMeters,
        hls: HlsMeters,
        sessions: Registry,
        store: StreamStore,
    ) -> Self {
        Self {
            meters,
            origin,
            hls,
            sessions,
            store,
            hooks: None,
            playback: None,
        }
    }

    /// Also exports delivery counters for the configured hooks.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Also exports viewer JWT denials labelled only by status.
    #[must_use]
    pub fn with_playback(mut self, meters: PlaybackMeters) -> Self {
        self.playback = Some(meters);
        self
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let retention = self.retention_depths();
        let published = self.store.leased();
        MetricsSnapshot {
            process: self.meters.snapshot(),
            origin: self.origin.snapshot(),
            hls: self.hls.snapshot(),
            // Count only: cloning every session is the `/metrics/streams` scrape.
            active_sessions: self.sessions.len(),
            published_streams: published,
            idle_streams: self.store.len() - published,
            // Bounded by what the operator configured, so this cannot grow the
            // way stream labels can and stays on the process scrape.
            hooks: self
                .hooks
                .as_ref()
                .map(Hooks::snapshots)
                .unwrap_or_default(),
            retained_payload_bytes: retention.iter().map(|(_, depth)| depth.memory_bytes).sum(),
            retained_disk_bytes: retention.iter().map(|(_, depth)| depth.disk_bytes).sum(),
            retention_memory_capacity: retention
                .iter()
                .map(|(_, depth)| depth.memory_capacity)
                .sum(),
            retention_disk_capacity: retention.iter().map(|(_, depth)| depth.disk_capacity).sum(),
            spill_pending: self.store.disk().map_or(0, |disk| disk.spill_pending()),
            spills_failed: self.store.disk().map_or(0, |disk| disk.spills_failed()),
            retention_requested: self.store.limits().retention.retain,
            retention,
            playback: self.playback.as_ref().map(PlaybackMeters::snapshot),
        }
    }

    fn retention_depths(&self) -> Vec<(StreamId, RetentionDepth)> {
        let mut retention = self.store.live_streams();
        retention.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        retention
            .into_iter()
            .map(|(id, live)| (id, live.retention_depth()))
            .collect()
    }
}

// Node-wide gauges that are not one component's counter bank.
series! {
    MetricsSnapshot {
        Gauge("rushls_active_sessions", "Publishing sessions currently active.")
            = |snapshot: &MetricsSnapshot| snapshot.active_sessions,
        Gauge("rushls_published_streams", "Streams with a publisher currently attached.")
            = |snapshot: &MetricsSnapshot| snapshot.published_streams,
        Gauge("rushls_idle_streams", "Retained streams waiting for a publisher to return.")
            = |snapshot: &MetricsSnapshot| snapshot.idle_streams,
        Gauge("rushls_retained_payload_bytes",
            "Media payload bytes currently retained for viewers in the memory tier.")
            = |snapshot: &MetricsSnapshot| snapshot.retained_payload_bytes,
        Gauge("rushls_retained_disk_bytes",
            "Media payload bytes currently retained for viewers in the disk tier.")
            = |snapshot: &MetricsSnapshot| snapshot.retained_disk_bytes,
        Gauge("rushls_retention_requested_seconds",
            "Configured retain window in seconds. Per-stream held duration is at /metrics/streams.")
            = |snapshot: &MetricsSnapshot| snapshot.retention_requested.as_secs_f64(),
        Gauge("rushls_disk_spill_pending",
            "Spill jobs accepted and not yet written. At capacity publishers wait for disk progress.")
            = |snapshot: &MetricsSnapshot| snapshot.spill_pending,
        Counter("rushls_disk_spills_failed_total",
            "Spill writes that failed. Media stayed in memory.")
            = |snapshot: &MetricsSnapshot| snapshot.spills_failed,
    }
}

/// Serializes one atomic snapshot in Prometheus' text exposition format.
///
/// What each series is called and what it means lives beside the counter it
/// reads, in `observe`. This decides only how to spell them on the wire, which
/// is why adding a counter no longer means editing this function.
pub fn render(snapshot: &MetricsSnapshot) -> String {
    let mut output = String::with_capacity(4_096);

    scalars(&mut output, ProcessSnapshot::SERIES, &snapshot.process);
    scalars(&mut output, OriginSnapshot::SERIES, &snapshot.origin);
    scalars(&mut output, HlsSnapshot::SERIES, &snapshot.hls);
    scalars(&mut output, MetricsSnapshot::SERIES, snapshot);
    render_retention_capacity(&mut output, snapshot);
    render_playback(&mut output, snapshot.playback);

    if !snapshot.hooks.is_empty() {
        render_hooks(&mut output, &snapshot.hooks);
    }
    output
}

/// Sessions and per-stream retention for `/metrics/streams`.
fn render_labelled(
    sessions: &[SessionSnapshot],
    retention: &[(StreamId, RetentionDepth)],
) -> String {
    let mut output = String::with_capacity(512 + retention.len() * 256 + sessions.len() * 2_048);
    render_retention_into(&mut output, retention);
    render_sessions(&mut output, sessions);
    output
}

/// One unlabelled reading per series, each under its own metadata.
fn scalars<S>(output: &mut String, series: &[Series<S>], source: &S) {
    for series in series {
        metadata(output, series.name, series.help, series.kind);
        writeln!(output, "{} {}", series.name, (series.read)(source))
            .expect("writing to a String cannot fail");
    }
}

const RETENTION_CAPACITY_BYTES: &str = "rushls_retention_capacity_bytes";

fn render_retention_capacity(output: &mut String, snapshot: &MetricsSnapshot) {
    metadata(
        output,
        RETENTION_CAPACITY_BYTES,
        "Configured retention cap across currently retained streams, per tier.",
        MetricKind::Gauge,
    );
    writeln!(
        output,
        "{RETENTION_CAPACITY_BYTES}{{tier=\"memory\"}} {}",
        snapshot.retention_memory_capacity
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        "{RETENTION_CAPACITY_BYTES}{{tier=\"disk\"}} {}",
        snapshot.retention_disk_capacity
    )
    .expect("writing to a String cannot fail");
}

const PLAYBACK_DENIED_TOTAL: &str = "rushls_playback_denied_total";

fn render_playback(output: &mut String, playback: Option<PlaybackDenials>) {
    let Some(denials) = playback else {
        return;
    };
    metadata(
        output,
        PLAYBACK_DENIED_TOTAL,
        "Viewer requests refused by playback authorization, labelled by HTTP status.",
        MetricKind::Counter,
    );
    writeln!(
        output,
        "{PLAYBACK_DENIED_TOTAL}{{status=\"401\"}} {}",
        denials.unauthorized
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        "{PLAYBACK_DENIED_TOTAL}{{status=\"403\"}} {}",
        denials.forbidden
    )
    .expect("writing to a String cannot fail");
}

/// One counter per hook, labelled by the name the operator configured.
fn render_hooks(output: &mut String, hooks: &[(Arc<str>, HookSnapshot)]) {
    for series in HOOK_SERIES {
        metadata(output, series.name, series.help, series.kind);
        for (hook, snapshot) in hooks {
            writeln!(
                output,
                "{}{{hook=\"{}\"}} {}",
                series.name,
                escape_label(hook),
                (series.read)(snapshot)
            )
            .expect("writing to a String cannot fail");
        }
    }
}

fn metadata(output: &mut String, name: &str, help: &str, kind: MetricKind) {
    writeln!(output, "# HELP {name} {help}").expect("writing to a String cannot fail");
    writeln!(output, "# TYPE {name} {}", kind.as_str()).expect("writing to a String cannot fail");
}

/// Per-stream series, each carrying the session's identity as labels.
///
/// Metadata for every series comes first and the readings follow, rather than
/// interleaving them per session: `# HELP` and `# TYPE` may each appear only
/// once for a metric, however many label sets it has. Emitted even when no
/// session is live, so the scrape still describes the series.
fn render_sessions(output: &mut String, sessions: &[SessionSnapshot]) {
    metadata(
        output,
        SESSION_INFO,
        "Identity and current lifecycle phase of an active session.",
        MetricKind::Gauge,
    );
    for series in MeterSnapshot::SERIES {
        metadata(output, series.name, series.help, series.kind);
    }
    metadata(
        output,
        SESSION_TRACKS,
        "Discovered tracks in an active session by media kind.",
        MetricKind::Gauge,
    );

    for session in sessions {
        let labels = session_labels(session);
        writeln!(
            output,
            "{SESSION_INFO}{{{labels},phase=\"{}\"}} 1",
            session.phase
        )
        .expect("writing to a String cannot fail");
        for series in MeterSnapshot::SERIES {
            writeln!(
                output,
                "{}{{{labels}}} {}",
                series.name,
                (series.read)(&session.meters)
            )
            .expect("writing to a String cannot fail");
        }
        for (kind, count) in [
            ("audio", session.tracks.audio),
            ("subtitle", session.tracks.subtitle),
            ("video", session.tracks.video),
        ] {
            writeln!(
                output,
                "{SESSION_TRACKS}{{{labels},kind=\"{kind}\"}} {count}"
            )
            .expect("writing to a String cannot fail");
        }
    }
}

/// Carries a label set no snapshot field can produce, so it is spelled here
/// rather than declared as a series.
const SESSION_INFO: &str = "rushls_session_info";
const SESSION_TRACKS: &str = "rushls_session_tracks";

fn session_labels(session: &SessionSnapshot) -> String {
    format!(
        "session=\"{}\",stream=\"{}\",principal=\"{}\"",
        escape_label(&session.id.to_string()),
        escape_label(session.stream.as_str()),
        escape_label(&session.principal.0)
    )
}

series! {
    STREAM_RETENTION_SERIES: RetentionDepth {
        Gauge("rushls_stream_retention_requested_seconds",
            "Configured retain for this stream, in seconds.")
            = |depth: &RetentionDepth| depth.requested.as_secs_f64(),
        Gauge("rushls_stream_retention_held_seconds",
            "Advertised playlist duration currently named for this stream, in seconds.")
            = |depth: &RetentionDepth| depth.held.as_secs_f64(),
    }
}

const STREAM_RETAINED_BYTES: &str = "rushls_stream_retained_bytes";
const STREAM_RETENTION_CAPACITY_BYTES: &str = "rushls_stream_retention_capacity_bytes";

fn render_retention_into(output: &mut String, retention: &[(StreamId, RetentionDepth)]) {
    for series in STREAM_RETENTION_SERIES {
        metadata(output, series.name, series.help, series.kind);
        for (stream, depth) in retention {
            writeln!(
                output,
                "{}{{stream=\"{}\"}} {}",
                series.name,
                escape_label(stream.as_str()),
                (series.read)(depth)
            )
            .expect("writing to a String cannot fail");
        }
    }
    metadata(
        output,
        STREAM_RETAINED_BYTES,
        "Payload bytes this stream holds in one retention tier.",
        MetricKind::Gauge,
    );
    metadata(
        output,
        STREAM_RETENTION_CAPACITY_BYTES,
        "Configured cap for one retention tier of this stream.",
        MetricKind::Gauge,
    );
    for (stream, depth) in retention {
        let stream = escape_label(stream.as_str());
        for tier in depth.tiers() {
            writeln!(
                output,
                "{STREAM_RETAINED_BYTES}{{stream=\"{stream}\",tier=\"{}\"}} {}",
                tier.name, tier.bytes
            )
            .expect("writing to a String cannot fail");
            writeln!(
                output,
                "{STREAM_RETENTION_CAPACITY_BYTES}{{stream=\"{stream}\",tier=\"{}\"}} {}",
                tier.name, tier.capacity
            )
            .expect("writing to a String cannot fail");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::{Principal, PublishGrant, StreamPolicy},
        domain::StreamId,
        observe::SessionMeters,
        session::StopToken,
    };

    use super::*;

    fn grant() -> PublishGrant {
        PublishGrant {
            stream_id: StreamId::new("live/camera"),
            principal: Principal("publisher".into()),
            policy: StreamPolicy::permissive(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn labelled_series_live_on_the_streams_scrape() {
        let meters = ProcessMeters::default();
        let sessions = Registry::default();
        let store = StreamStore::default();
        meters.session_started();

        let session_meters = SessionMeters::new(meters.clone());
        session_meters.source_view().source_progress(512, 4, 0);
        let registration = sessions
            .register(&grant(), session_meters, StopToken::new())
            .expect("the registry has room");
        let lease = store
            .lease_without_presentation(StreamId::new("live/camera"))
            .expect("the store has room");

        let endpoint = MetricsEndpoint::new(
            MetricsReader::new(
                meters.clone(),
                OriginMeters::default(),
                HlsMeters::default(),
                sessions.clone(),
                store.clone(),
            ),
            None,
        );

        let totals = endpoint.render();
        assert!(totals.contains("rushls_active_sessions 1\n"));
        assert!(
            !totals.contains("rushls_session_info"),
            "session identity belongs on /metrics/streams"
        );
        assert!(
            !totals.contains("stream=\"live/camera\""),
            "stream labels belong on /metrics/streams"
        );

        let labelled = endpoint.render_streams();
        assert!(labelled.contains("stream=\"live/camera\""));
        assert!(labelled.contains("principal=\"publisher\""));
        assert!(labelled.contains("rushls_session_info{"));
        assert!(labelled.contains("rushls_session_bytes_received_total{"));
        assert!(labelled.contains(" 512\n"));
        assert!(
            labelled.contains("rushls_stream_retention_requested_seconds{stream=\"live/camera\"}")
        );
        assert!(
            !labelled.contains("rushls_active_sessions"),
            "/metrics/streams is labelled series, not process totals"
        );

        drop(registration);
        drop(lease);

        let idle = MetricsReader::new(
            meters,
            OriginMeters::default(),
            HlsMeters::default(),
            sessions,
            store,
        )
        .snapshot();
        assert_eq!(idle.active_sessions, 0);
        assert_eq!(
            (idle.published_streams, idle.idle_streams),
            (0, 1),
            "a stream awaiting reconnection is retained but not counted as published"
        );
        assert_eq!(idle.process.sessions_started, 1);
        assert_eq!(idle.process.bytes_received, 512);
        assert_eq!(idle.retention.len(), 1, "idle streams still occupy retain");
        assert_eq!(idle.retention[0].0.as_str(), "live/camera");
        assert_eq!(
            idle.retention_requested, idle.retention[0].1.requested,
            "the process total is the same request each stream was given"
        );
    }

    #[test]
    fn endpoint_authentication_is_optional_and_exact() {
        let reader = MetricsReader::new(
            ProcessMeters::default(),
            OriginMeters::default(),
            HlsMeters::default(),
            Registry::default(),
            StreamStore::default(),
        );
        let open = MetricsEndpoint::new(reader.clone(), None);
        assert!(open.authorize(None));

        let protected = MetricsEndpoint::new(reader, Some(MetricsToken::new("scrape-secret")));
        assert!(protected.authorize(Some("scrape-secret")));
        assert!(!protected.authorize(None));
        assert!(!protected.authorize(Some("scrape")));
        assert!(!protected.authorize(Some("scrape-secret ")));
    }

    #[test]
    fn prometheus_output_contains_typed_process_metrics() {
        let output = render(&MetricsSnapshot {
            process: ProcessSnapshot {
                sessions_started: 3,
                bytes_received: 1_024,
                ..ProcessSnapshot::default()
            },
            origin: OriginSnapshot::default(),
            hls: HlsSnapshot::default(),
            active_sessions: 2,
            published_streams: 1,
            idle_streams: 4,
            hooks: Vec::new(),
            retained_payload_bytes: 0,
            retained_disk_bytes: 0,
            retention_memory_capacity: 0,
            retention_disk_capacity: 0,
            spill_pending: 0,
            spills_failed: 0,
            retention_requested: Duration::ZERO,
            retention: Vec::new(),
            playback: None,
        });

        assert!(output.contains("# TYPE rushls_sessions_started_total counter\n"));
        assert!(output.contains("rushls_sessions_started_total 3\n"));
        assert!(output.contains("rushls_bytes_received_total 1024\n"));
        assert!(output.contains("# TYPE rushls_active_sessions gauge\n"));
        assert!(output.contains("rushls_active_sessions 2\n"));
        assert!(output.contains("rushls_retained_payload_bytes 0\n"));
        assert!(output.contains("rushls_retained_disk_bytes 0\n"));
        assert!(output.contains("rushls_retention_requested_seconds 0\n"));
        assert!(output.contains("rushls_disk_spill_pending 0\n"));
        assert!(output.contains("# TYPE rushls_disk_spills_failed_total counter\n"));
        assert!(output.contains("rushls_disk_spills_failed_total 0\n"));
        assert!(output.contains("rushls_retention_capacity_bytes{tier=\"memory\"} 0\n"));
        assert!(output.contains("rushls_retention_capacity_bytes{tier=\"disk\"} 0\n"));
        assert!(
            !output.contains("rushls_stream_retention_"),
            "per-stream retention belongs on /metrics/streams"
        );
        assert!(
            !output.contains("rushls_session_info"),
            "session series belong on /metrics/streams"
        );
        assert!(
            !output.contains("rushls_hook_"),
            "a node with no hooks exports no hook series at all, rather than \
             zeroes an operator would have to learn to ignore"
        );
        assert!(
            !output.contains("rushls_playback_denied_total"),
            "playback denials are absent until [auth.playback] is configured"
        );
    }

    #[test]
    fn every_delivery_meter_is_exported() {
        let origin = OriginMeters::default();
        origin.media_served(2_048);
        origin.request_rejected();
        origin.request_not_found();
        let hls = HlsMeters::default();
        hls.playlist_served(true);
        hls.playlist_served(false);
        hls.blocking_reload_started();
        hls.blocking_reload_expired();

        let output = MetricsEndpoint::new(
            MetricsReader::new(
                ProcessMeters::default(),
                origin,
                hls,
                Registry::default(),
                StreamStore::default(),
            ),
            None,
        )
        .render();

        assert!(output.contains("rushls_media_responses_served_total 1\n"));
        assert!(output.contains("rushls_bytes_served_total 2048\n"));
        assert!(output.contains("rushls_origin_requests_rejected_total 1\n"));
        assert!(output.contains("rushls_origin_requests_not_found_total 1\n"));
        assert!(output.contains("rushls_hls_playlists_served_total 2\n"));
        assert!(output.contains("rushls_hls_playlists_rendered_total 1\n"));
        assert!(output.contains("rushls_hls_blocking_reloads_total 1\n"));
        assert!(output.contains("rushls_hls_blocking_reloads_expired_total 1\n"));
    }

    #[test]
    fn every_way_a_hook_can_lose_an_event_is_exported_separately() {
        let output = render(&MetricsSnapshot {
            process: ProcessSnapshot::default(),
            origin: OriginSnapshot::default(),
            hls: HlsSnapshot::default(),
            active_sessions: 0,
            published_streams: 0,
            idle_streams: 0,
            hooks: vec![
                (
                    Arc::from("automation"),
                    HookSnapshot {
                        delivered: 41,
                        retried: 2,
                        ingress: 8,
                        overflow: 3,
                        rejected: 4,
                        exhausted: 5,
                        shutdown: 6,
                        outcome_unknown_shutdown: 9,
                        filtered: 7,
                        ingress_depth: 10,
                        ingress_capacity: 11,
                        queue_depth: 12,
                        queue_capacity: 13,
                        in_flight: 14,
                    },
                ),
                (Arc::from("audit"), HookSnapshot::default()),
            ],
            retained_payload_bytes: 0,
            retained_disk_bytes: 0,
            retention_memory_capacity: 0,
            retention_disk_capacity: 0,
            spill_pending: 0,
            spills_failed: 0,
            retention_requested: Duration::ZERO,
            retention: Vec::new(),
            playback: None,
        });

        assert!(output.contains("# TYPE rushls_hook_deliveries_total counter\n"));
        assert!(output.contains("rushls_hook_deliveries_total{hook=\"automation\"} 41\n"));
        assert!(output.contains("rushls_hook_deliveries_total{hook=\"audit\"} 0\n"));
        // Kept apart because an endpoint refusing an event, a queue overflowing,
        // and a shutdown cutting a drain short call for different responses.
        assert!(output.contains("rushls_hook_dropped_overflow_total{hook=\"automation\"} 3\n"));
        assert!(output.contains("rushls_hook_dropped_rejected_total{hook=\"automation\"} 4\n"));
        assert!(output.contains("rushls_hook_dropped_exhausted_total{hook=\"automation\"} 5\n"));
        assert!(output.contains("rushls_hook_dropped_shutdown_total{hook=\"automation\"} 6\n"));
        assert!(output.contains("rushls_hook_dropped_ingress_total{hook=\"automation\"} 8\n"));
        assert!(
            output.contains("rushls_hook_outcome_unknown_shutdown_total{hook=\"automation\"} 9\n")
        );
        assert!(output.contains("rushls_hook_retries_total{hook=\"automation\"} 2\n"));
        assert!(output.contains("rushls_hook_filtered_total{hook=\"automation\"} 7\n"));
        assert!(output.contains("rushls_hook_ingress_depth{hook=\"automation\"} 10\n"));
        assert!(output.contains("rushls_hook_ingress_capacity{hook=\"automation\"} 11\n"));
        assert!(output.contains("rushls_hook_queue_depth{hook=\"automation\"} 12\n"));
        assert!(output.contains("rushls_hook_queue_capacity{hook=\"automation\"} 13\n"));
        assert!(output.contains("rushls_hook_in_flight{hook=\"automation\"} 14\n"));

        assert_eq!(
            output
                .matches("# TYPE rushls_hook_deliveries_total")
                .count(),
            1,
            "one HELP and TYPE per metric, with hooks as labels beneath it"
        );
    }

    #[test]
    fn stream_retention_is_labelled_and_tiered() {
        let output = render_labelled(
            &[],
            &[(
                StreamId::new("live/camera"),
                RetentionDepth {
                    requested: Duration::from_mins(2),
                    held: Duration::from_secs(42),
                    memory_bytes: 4_096,
                    memory_capacity: 256 * 1024 * 1024,
                    disk_bytes: 0,
                    disk_capacity: 0,
                },
            )],
        );

        assert!(
            output.contains(
                "rushls_stream_retention_requested_seconds{stream=\"live/camera\"} 120\n"
            )
        );
        assert!(
            output.contains("rushls_stream_retention_held_seconds{stream=\"live/camera\"} 42\n")
        );
        assert!(output.contains(
            "rushls_stream_retained_bytes{stream=\"live/camera\",tier=\"memory\"} 4096\n"
        ));
        assert!(output.contains(
            "rushls_stream_retention_capacity_bytes{stream=\"live/camera\",tier=\"memory\"} 268435456\n"
        ));
        assert!(
            output
                .contains("rushls_stream_retained_bytes{stream=\"live/camera\",tier=\"disk\"} 0\n")
        );
        assert!(output.contains(
            "rushls_stream_retention_capacity_bytes{stream=\"live/camera\",tier=\"disk\"} 0\n"
        ));
        assert!(
            !output.contains("rushls_active_sessions"),
            "/metrics/streams is labelled series, not process totals"
        );
    }

    #[test]
    fn process_dvr_gauges_are_labelled_by_tier() {
        let output = render(&MetricsSnapshot {
            process: ProcessSnapshot::default(),
            origin: OriginSnapshot::default(),
            hls: HlsSnapshot::default(),
            active_sessions: 0,
            published_streams: 0,
            idle_streams: 1,
            hooks: Vec::new(),
            retained_payload_bytes: 4_096,
            retained_disk_bytes: 512,
            retention_memory_capacity: 256 * 1024 * 1024,
            retention_disk_capacity: 8 * 1024 * 1024 * 1024,
            spill_pending: 3,
            spills_failed: 4,
            retention_requested: Duration::from_mins(2),
            retention: Vec::new(),
            playback: None,
        });

        assert!(output.contains("rushls_disk_spill_pending 3\n"));
        assert!(output.contains("rushls_disk_spills_failed_total 4\n"));
        assert!(output.contains("rushls_retention_capacity_bytes{tier=\"memory\"} 268435456\n"));
        assert!(output.contains("rushls_retention_capacity_bytes{tier=\"disk\"} 8589934592\n"));
        assert!(
            !output.contains("rushls_stream_retained_bytes"),
            "per-stream series stay on /metrics/streams"
        );
    }

    #[test]
    fn an_empty_store_still_describes_the_labelled_series() {
        let output = render_labelled(&[], &[]);

        assert!(output.contains("# TYPE rushls_stream_retention_held_seconds gauge\n"));
        assert!(output.contains("# TYPE rushls_stream_retained_bytes gauge\n"));
        assert!(output.contains("# TYPE rushls_session_info gauge\n"));
        assert!(
            !output.contains("{stream="),
            "no samples until a stream is retained"
        );
    }
}
