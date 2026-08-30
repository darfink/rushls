use std::{fmt::Write, sync::Arc};

use subtle::ConstantTimeEq;

use crate::{
    delivery::hls::StreamStore,
    hooks::{HOOK_SERIES, HookSnapshot, Hooks},
    observe::{
        HlsMeters, HlsSnapshot, MeterSnapshot, MetricKind, OriginMeters, OriginSnapshot,
        ProcessMeters, ProcessSnapshot, Series, counters::series,
    },
    session::{Registry, SessionSnapshot},
};

#[derive(Clone, Eq, PartialEq, derive_more::Debug)]
#[debug("MetricsToken([REDACTED])")]
pub struct MetricsToken(Vec<u8>);

impl MetricsToken {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    /// Whether this token could never be presented as a bearer credential.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn matches(&self, presented: &[u8]) -> bool {
        self.0.as_slice().ct_eq(presented).into()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportPolicy {
    /// Whether per-stream series are exported alongside process totals.
    ///
    /// Off by default: stream identity is unbounded cardinality, which is a
    /// good way to take down a metrics backend.
    pub per_stream: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetricsConfig {
    /// Whether the HTTP server exposes `/metrics`.
    pub enabled: bool,
    /// When present, scrapes must authenticate with this bearer token.
    pub token: Option<MetricsToken>,
    pub export: ExportPolicy,
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
    pub streams: Vec<SessionSnapshot>,
    /// Delivery counters per configured hook.
    pub hooks: Vec<(Arc<str>, HookSnapshot)>,
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
    policy: ExportPolicy,
}

impl MetricsReader {
    pub fn new(
        meters: ProcessMeters,
        origin: OriginMeters,
        hls: HlsMeters,
        sessions: Registry,
        store: StreamStore,
        policy: ExportPolicy,
    ) -> Self {
        Self {
            meters,
            origin,
            hls,
            sessions,
            store,
            hooks: None,
            policy,
        }
    }

    /// Also exports delivery counters for the configured hooks.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let sessions = self.sessions.snapshot();
        let published = self.store.leased();
        MetricsSnapshot {
            process: self.meters.snapshot(),
            origin: self.origin.snapshot(),
            hls: self.hls.snapshot(),
            active_sessions: sessions.len(),
            published_streams: published,
            idle_streams: self.store.len() - published,
            streams: if self.policy.per_stream {
                sessions
            } else {
                Vec::new()
            },
            // Always exported, unlike per-stream series: the number of hooks is
            // what an operator configured, so this cannot grow unboundedly the
            // way stream labels can.
            hooks: self
                .hooks
                .as_ref()
                .map(Hooks::snapshots)
                .unwrap_or_default(),
        }
    }
}

// The three readings that belong to the node itself rather than to any one
// component's counters.
series! {
    MetricsSnapshot {
        Gauge("rushls_active_sessions", "Publishing sessions currently active.")
            = |snapshot: &MetricsSnapshot| snapshot.active_sessions,
        Gauge("rushls_published_streams", "Streams with a publisher currently attached.")
            = |snapshot: &MetricsSnapshot| snapshot.published_streams,
        Gauge("rushls_idle_streams", "Retained streams waiting for a publisher to return.")
            = |snapshot: &MetricsSnapshot| snapshot.idle_streams,
    }
}

/// Serializes one atomic snapshot in Prometheus' text exposition format.
///
/// What each series is called and what it means lives beside the counter it
/// reads, in `observe`. This decides only how to spell them on the wire, which
/// is why adding a counter no longer means editing this function.
pub fn render(snapshot: &MetricsSnapshot) -> String {
    let mut output = String::with_capacity(4_096 + snapshot.streams.len() * 2_048);

    scalars(&mut output, ProcessSnapshot::SERIES, &snapshot.process);
    scalars(&mut output, OriginSnapshot::SERIES, &snapshot.origin);
    scalars(&mut output, HlsSnapshot::SERIES, &snapshot.hls);
    scalars(&mut output, MetricsSnapshot::SERIES, snapshot);

    if !snapshot.hooks.is_empty() {
        render_hooks(&mut output, &snapshot.hooks);
    }
    if !snapshot.streams.is_empty() {
        render_sessions(&mut output, &snapshot.streams);
    }
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
/// once for a metric, however many label sets it has.
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

fn escape_label(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('\n', r"\n")
        .replace('"', r#"\""#)
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
    async fn per_stream_series_are_optional_while_totals_are_always_exported() {
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

        let detailed = MetricsReader::new(
            meters.clone(),
            OriginMeters::default(),
            HlsMeters::default(),
            sessions.clone(),
            store.clone(),
            ExportPolicy { per_stream: true },
        )
        .snapshot();
        assert_eq!(detailed.active_sessions, 1);
        assert_eq!(detailed.published_streams, 1);
        assert_eq!(detailed.idle_streams, 0);
        assert_eq!(detailed.streams.len(), 1);
        assert_eq!(detailed.streams[0].meters.bytes_received, 512);

        drop(registration);
        drop(lease);

        let terse = MetricsReader::new(
            meters,
            OriginMeters::default(),
            HlsMeters::default(),
            sessions,
            store,
            ExportPolicy::default(),
        )
        .snapshot();
        assert_eq!(terse.active_sessions, 0);
        assert_eq!(
            (terse.published_streams, terse.idle_streams),
            (0, 1),
            "a stream awaiting reconnection is retained but not counted as published"
        );
        assert!(terse.streams.is_empty());
        assert_eq!(terse.process.sessions_started, 1);
        assert_eq!(terse.process.bytes_received, 512);
    }

    #[test]
    fn endpoint_authentication_is_optional_and_exact() {
        let reader = MetricsReader::new(
            ProcessMeters::default(),
            OriginMeters::default(),
            HlsMeters::default(),
            Registry::default(),
            StreamStore::default(),
            ExportPolicy::default(),
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
            streams: Vec::new(),
            hooks: Vec::new(),
        });

        assert!(output.contains("# TYPE rushls_sessions_started_total counter\n"));
        assert!(output.contains("rushls_sessions_started_total 3\n"));
        assert!(output.contains("rushls_bytes_received_total 1024\n"));
        assert!(output.contains("# TYPE rushls_active_sessions gauge\n"));
        assert!(output.contains("rushls_active_sessions 2\n"));
        assert!(
            !output.contains("rushls_hook_"),
            "a node with no hooks exports no hook series at all, rather than \
             zeroes an operator would have to learn to ignore"
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
                ExportPolicy::default(),
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
            streams: Vec::new(),
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
}
