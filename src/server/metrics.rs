use std::{fmt::Write, sync::Arc};

use subtle::ConstantTimeEq;

use crate::{
    delivery::hls::StreamStore,
    hooks::{HookSnapshot, Hooks},
    observe::{ProcessMeters, ProcessSnapshot},
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
        sessions: Registry,
        store: StreamStore,
        policy: ExportPolicy,
    ) -> Self {
        Self {
            meters,
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

/// Serializes one atomic snapshot in Prometheus' text exposition format.
pub fn render(snapshot: &MetricsSnapshot) -> String {
    let mut output = String::with_capacity(4_096 + snapshot.streams.len() * 2_048);
    let process = snapshot.process;

    counter(
        &mut output,
        "rushls_sessions_started_total",
        "Publishing sessions started.",
        process.sessions_started,
    );
    counter(
        &mut output,
        "rushls_sessions_completed_total",
        "Publishing sessions completed successfully.",
        process.sessions_completed,
    );
    counter(
        &mut output,
        "rushls_sessions_failed_total",
        "Publishing sessions that failed.",
        process.sessions_failed,
    );
    counter(
        &mut output,
        "rushls_sessions_replaced_total",
        "Publishing sessions displaced by a takeover.",
        process.sessions_replaced,
    );
    counter(
        &mut output,
        "rushls_publishers_rejected_total",
        "Publishers rejected before a session started.",
        process.publishers_rejected,
    );
    counter(
        &mut output,
        "rushls_codec_parameter_changes_total",
        "Mid-stream codec parameter changes detected.",
        process.codec_parameter_changes,
    );
    counter(
        &mut output,
        "rushls_unhealthy_terminations_total",
        "Sessions stopped by health supervision.",
        process.unhealthy_terminations,
    );
    counter(
        &mut output,
        "rushls_drain_failures_total",
        "Sessions that failed while flushing their tail.",
        process.drain_failures,
    );
    counter(
        &mut output,
        "rushls_bytes_received_total",
        "Bytes received from publishers.",
        process.bytes_received,
    );
    counter(
        &mut output,
        "rushls_packets_received_total",
        "Packets received from publishers.",
        process.packets_received,
    );
    counter(
        &mut output,
        "rushls_packets_lost_total",
        "Publisher packets reported lost.",
        process.packets_lost,
    );
    counter(
        &mut output,
        "rushls_parts_published_total",
        "HLS parts made available to viewers.",
        process.parts_published,
    );
    counter(
        &mut output,
        "rushls_segments_published_total",
        "HLS segments made available to viewers.",
        process.segments_published,
    );
    counter(
        &mut output,
        "rushls_tls_handshakes_completed_total",
        "TLS handshakes completed.",
        process.tls_handshakes_completed,
    );
    counter(
        &mut output,
        "rushls_tls_handshakes_failed_total",
        "TLS handshakes that failed or timed out.",
        process.tls_handshakes_failed,
    );
    gauge(
        &mut output,
        "rushls_active_sessions",
        "Publishing sessions currently active.",
        snapshot.active_sessions,
    );
    gauge(
        &mut output,
        "rushls_published_streams",
        "Streams with a publisher currently attached.",
        snapshot.published_streams,
    );
    gauge(
        &mut output,
        "rushls_idle_streams",
        "Retained streams waiting for a publisher to return.",
        snapshot.idle_streams,
    );

    if !snapshot.hooks.is_empty() {
        render_hooks(&mut output, &snapshot.hooks);
    }
    if !snapshot.streams.is_empty() {
        render_sessions(&mut output, &snapshot.streams);
    }
    output
}

/// One counter per hook, labelled by the name the operator configured.
///
/// Losses are kept apart rather than summed into one "failed" counter: an
/// endpoint refusing an event, a queue overflowing, and a shutdown cutting a
/// drain short call for three different responses from whoever is looking.
fn render_hooks(output: &mut String, hooks: &[(Arc<str>, HookSnapshot)]) {
    for (name, help, read) in [
        (
            "rushls_hook_deliveries_total",
            "Lifecycle events accepted by a hook endpoint.",
            (|snapshot: &HookSnapshot| snapshot.delivered) as fn(&HookSnapshot) -> u64,
        ),
        (
            "rushls_hook_retries_total",
            "Delivery attempts that failed and were retried.",
            |snapshot| snapshot.retried,
        ),
        (
            "rushls_hook_filtered_total",
            "Events not delivered because the hook did not subscribe to them.",
            |snapshot| snapshot.filtered,
        ),
        (
            "rushls_hook_dropped_overflow_total",
            "Events dropped because the hook's queue was full.",
            |snapshot| snapshot.overflow,
        ),
        (
            "rushls_hook_dropped_rejected_total",
            "Events refused by the endpoint in a way retrying cannot fix.",
            |snapshot| snapshot.rejected,
        ),
        (
            "rushls_hook_dropped_exhausted_total",
            "Events dropped after every delivery attempt failed.",
            |snapshot| snapshot.exhausted,
        ),
        (
            "rushls_hook_dropped_shutdown_total",
            "Events still queued when the drain deadline passed.",
            |snapshot| snapshot.shutdown,
        ),
    ] {
        metadata(output, name, help, "counter");
        for (hook, snapshot) in hooks {
            writeln!(
                output,
                "{name}{{hook=\"{}\"}} {}",
                escape_label(hook),
                read(snapshot)
            )
            .expect("writing to a String cannot fail");
        }
    }
}

fn counter(output: &mut String, name: &str, help: &str, value: u64) {
    metadata(output, name, help, "counter");
    writeln!(output, "{name} {value}").expect("writing to a String cannot fail");
}

fn gauge(output: &mut String, name: &str, help: &str, value: usize) {
    metadata(output, name, help, "gauge");
    writeln!(output, "{name} {value}").expect("writing to a String cannot fail");
}

fn metadata(output: &mut String, name: &str, help: &str, kind: &str) {
    writeln!(output, "# HELP {name} {help}").expect("writing to a String cannot fail");
    writeln!(output, "# TYPE {name} {kind}").expect("writing to a String cannot fail");
}

fn render_sessions(output: &mut String, sessions: &[SessionSnapshot]) {
    for (name, help, kind) in [
        (
            "rushls_session_info",
            "Identity and current lifecycle phase of an active session.",
            "gauge",
        ),
        (
            "rushls_session_bytes_received_total",
            "Bytes received by an active session.",
            "counter",
        ),
        (
            "rushls_session_packets_received_total",
            "Packets received by an active session.",
            "counter",
        ),
        (
            "rushls_session_packets_lost_total",
            "Packets reported lost by an active session.",
            "counter",
        ),
        (
            "rushls_session_packets_normalized_total",
            "Packets normalized by an active session.",
            "counter",
        ),
        (
            "rushls_session_samples_normalized_total",
            "Samples emitted by normalization for an active session.",
            "counter",
        ),
        (
            "rushls_session_chunks_muxed_total",
            "Media chunks muxed by an active session.",
            "counter",
        ),
        (
            "rushls_session_segments_muxed_total",
            "Segments muxed by an active session.",
            "counter",
        ),
        (
            "rushls_session_parts_published_total",
            "HLS parts published by an active session.",
            "counter",
        ),
        (
            "rushls_session_segments_published_total",
            "HLS segments published by an active session.",
            "counter",
        ),
        (
            "rushls_session_peak_packets_per_batch",
            "Largest packet batch observed by an active session.",
            "gauge",
        ),
        (
            "rushls_session_peak_samples_per_batch",
            "Largest sample batch observed by an active session.",
            "gauge",
        ),
        (
            "rushls_session_media_lead_seconds",
            "Current normalized-media lead for an active session.",
            "gauge",
        ),
        (
            "rushls_session_pacing_delay_seconds_total",
            "Pacing delay accumulated by an active session.",
            "counter",
        ),
        (
            "rushls_session_publisher_backpressured",
            "Whether an active publisher is currently backpressured.",
            "gauge",
        ),
        (
            "rushls_session_tracks",
            "Discovered tracks in an active session by media kind.",
            "gauge",
        ),
    ] {
        metadata(output, name, help, kind);
    }

    for session in sessions {
        let labels = session_labels(session);
        writeln!(
            output,
            "rushls_session_info{{{labels},phase=\"{}\"}} 1",
            session.phase
        )
        .expect("writing to a String cannot fail");
        session_metric(
            output,
            "rushls_session_bytes_received_total",
            &labels,
            session.meters.bytes_received,
        );
        session_metric(
            output,
            "rushls_session_packets_received_total",
            &labels,
            session.meters.packets_received,
        );
        session_metric(
            output,
            "rushls_session_packets_lost_total",
            &labels,
            session.meters.packets_lost,
        );
        session_metric(
            output,
            "rushls_session_packets_normalized_total",
            &labels,
            session.meters.packets_normalized,
        );
        session_metric(
            output,
            "rushls_session_samples_normalized_total",
            &labels,
            session.meters.samples_normalized,
        );
        session_metric(
            output,
            "rushls_session_chunks_muxed_total",
            &labels,
            session.meters.chunks_muxed,
        );
        session_metric(
            output,
            "rushls_session_segments_muxed_total",
            &labels,
            session.meters.segments_muxed,
        );
        session_metric(
            output,
            "rushls_session_parts_published_total",
            &labels,
            session.meters.parts_published,
        );
        session_metric(
            output,
            "rushls_session_segments_published_total",
            &labels,
            session.meters.segments_published,
        );
        session_metric(
            output,
            "rushls_session_peak_packets_per_batch",
            &labels,
            session.meters.peak_packets_per_batch,
        );
        session_metric(
            output,
            "rushls_session_peak_samples_per_batch",
            &labels,
            session.meters.peak_samples_per_batch,
        );
        session_float_metric(
            output,
            "rushls_session_media_lead_seconds",
            &labels,
            session.meters.media_lead.as_secs_f64(),
        );
        session_float_metric(
            output,
            "rushls_session_pacing_delay_seconds_total",
            &labels,
            session.meters.pacing_delay.as_secs_f64(),
        );
        session_metric(
            output,
            "rushls_session_publisher_backpressured",
            &labels,
            u64::from(session.meters.publisher_backpressured),
        );
        for (kind, count) in [
            ("audio", session.tracks.audio),
            ("subtitle", session.tracks.subtitle),
            ("video", session.tracks.video),
        ] {
            writeln!(
                output,
                "rushls_session_tracks{{{labels},kind=\"{kind}\"}} {count}"
            )
            .expect("writing to a String cannot fail");
        }
    }
}

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

fn session_metric(output: &mut String, name: &str, labels: &str, value: u64) {
    writeln!(output, "{name}{{{labels}}} {value}").expect("writing to a String cannot fail");
}

fn session_float_metric(output: &mut String, name: &str, labels: &str, value: f64) {
    writeln!(output, "{name}{{{labels}}} {value}").expect("writing to a String cannot fail");
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

        let terse = MetricsReader::new(meters, sessions, store, ExportPolicy::default()).snapshot();
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
    fn every_way_a_hook_can_lose_an_event_is_exported_separately() {
        let output = render(&MetricsSnapshot {
            process: ProcessSnapshot::default(),
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
                        overflow: 3,
                        rejected: 4,
                        exhausted: 5,
                        shutdown: 6,
                        filtered: 7,
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
        assert!(output.contains("rushls_hook_retries_total{hook=\"automation\"} 2\n"));
        assert!(output.contains("rushls_hook_filtered_total{hook=\"automation\"} 7\n"));

        assert_eq!(
            output
                .matches("# TYPE rushls_hook_deliveries_total")
                .count(),
            1,
            "one HELP and TYPE per metric, with hooks as labels beneath it"
        );
    }
}
