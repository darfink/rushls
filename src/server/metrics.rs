use crate::{
    delivery::hls::StreamStore,
    observe::{ProcessMeters, ProcessSnapshot},
    session::{Registry, SessionSnapshot},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportPolicy {
    /// Whether per-stream series are exported alongside process totals.
    ///
    /// Off by default: stream identity is unbounded cardinality, which is a
    /// good way to take down a metrics backend.
    pub per_stream: bool,
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
            policy,
        }
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
            .lease(StreamId::new("live/camera"))
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
}
