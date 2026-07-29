use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use derive_more::{Debug, Display};

use crate::domain::{SessionId, StreamId, TrackCounts, TrackId};

/// A rare, structured fact about a session's progress through its lifecycle.
///
/// Payloads stay primitive so this layer never depends on a domain aggregate.
/// Anything that happens per packet, sample, or part belongs in
/// [`SessionMeters`](super::SessionMeters) instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Accepted {
        stream: StreamId,
        principal: String,
    },
    /// A previously running session lost this stream to a new publisher.
    Displaced {
        stream: StreamId,
    },
    TracksDiscovered {
        counts: TrackCounts,
    },
    TimelineCalibrated {
        authority: TrackId,
    },
    /// Emitted once, when segmentation timing becomes immutable.
    SegmentationLocked {
        segment: Duration,
        part: Duration,
    },
    /// A non-strict muxer kept parts flowing while extending a segment to a
    /// usable random-access boundary.
    SegmentationExtended {
        track: TrackId,
        planned: Duration,
        actual: Duration,
    },
    Running,
    TrackSetChanged,
    CodecParametersChanged {
        track: TrackId,
    },
    Unhealthy {
        reason: String,
    },
    Draining,
    /// The session ran, but its trailing media could not be flushed.
    ///
    /// Not a [`Self::Failed`]: everything published before the drain is intact
    /// and the session still reached a real outcome. Reported separately so a
    /// muxer that consistently cannot finalize is visible rather than buried in
    /// the ordinary end-of-session noise.
    DrainFailed {
        reason: String,
    },
    Ended {
        end: SessionEnd,
    },
    Failed {
        reason: String,
    },
}

/// Why a session stopped.
///
/// Distinct from `SessionOutcome`, which only describes the ways a session can
/// stop *successfully*. Observability needs the failure shapes too.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "lowercase")]
pub enum SessionEnd {
    Ended,
    Interrupted,
    Replaced,
    Cancelled,
    Unhealthy,
    Failed,
}

/// A rare, structured fact about the process rather than about one session.
///
/// The same rule that separates [`SessionEvent`] from
/// [`SessionMeters`](super::SessionMeters) applies here, and TLS is the clean
/// illustration of both sides of it: a certificate rotation is a rare
/// structured fact and belongs here, while a failed handshake happens as often
/// as a remote peer decides it should and belongs in
/// [`ProcessMeters`](super::ProcessMeters).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeEvent {
    /// Carries the address that was actually bound, which with an ephemeral
    /// port is the only place it exists.
    ListenerBound {
        protocol: Protocol,
        address: SocketAddr,
    },
    /// A certificate and key became the pair new handshakes are answered with.
    ///
    /// Emitted for the initial load as well as every reload, so "which
    /// certificate is this process serving" is answerable from the event
    /// stream alone.
    CertificateLoaded { certificate: PathBuf },
    /// A rotation was seen but not adopted; the previous pair still serves.
    CertificateRejected {
        certificate: PathBuf,
        reason: String,
    },
    /// Rotations will no longer be noticed, while TLS keeps working.
    ///
    /// Reported separately because it is otherwise invisible: nothing breaks
    /// until the loaded certificate expires, by which point the cause is long
    /// out of the logs.
    CertificateWatchLost { reason: String },
}

/// Which listener an event is about.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "UPPERCASE")]
pub enum Protocol {
    Rtmp,
    Srt,
    Http,
    Https,
}

pub trait EventObserver: Send + Sync {
    fn observe(&self, session: SessionId, event: SessionEvent);

    /// Defaulted so an observer that only cares about sessions stays a
    /// one-method impl.
    fn observe_node(&self, _event: NodeEvent) {}
}

/// The process-wide event destination.
#[derive(Clone, Debug)]
#[debug("Events")]
pub struct Events(Arc<dyn EventObserver>);

impl Events {
    pub fn new(observer: Arc<dyn EventObserver>) -> Self {
        Self(observer)
    }

    /// Binds the destination to one session so stages emit without repeating
    /// the identity at every call site.
    pub fn scoped(&self, session: SessionId) -> EventSink {
        EventSink {
            observer: Arc::clone(&self.0),
            session,
        }
    }

    /// Reports a fact about the process, which has no session to scope to.
    pub fn emit(&self, event: NodeEvent) {
        self.0.observe_node(event);
    }
}

impl Default for Events {
    fn default() -> Self {
        Self(Arc::new(Discard))
    }
}

/// A session-scoped event outlet, cheap to clone into any stage.
#[derive(Clone, Debug)]
#[debug("EventSink {{ session: {session:?} }}")]
pub struct EventSink {
    #[debug(skip)]
    observer: Arc<dyn EventObserver>,
    session: SessionId,
}

impl EventSink {
    pub fn emit(&self, event: SessionEvent) {
        self.observer.observe(self.session, event);
    }

    pub fn session(&self) -> SessionId {
        self.session
    }
}

struct Discard;

impl EventObserver for Discard {
    fn observe(&self, _session: SessionId, _event: SessionEvent) {}
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<(SessionId, SessionEvent)>>,
        node: Mutex<Vec<NodeEvent>>,
    }

    impl EventObserver for Recorder {
        fn observe(&self, session: SessionId, event: SessionEvent) {
            self.events.lock().push((session, event));
        }

        fn observe_node(&self, event: NodeEvent) {
            self.node.lock().push(event);
        }
    }

    #[test]
    fn scoped_sinks_carry_their_session_identity() {
        let recorder = Arc::new(Recorder::default());
        let events = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>);
        let id = SessionId(nz::u64!(7));

        events.scoped(id).emit(SessionEvent::Running);

        let observed = recorder.events.lock();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0], (id, SessionEvent::Running));
    }

    #[test]
    fn process_events_reach_the_same_destination_without_a_session() {
        let recorder = Arc::new(Recorder::default());
        let events = Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>);

        events.emit(NodeEvent::CertificateLoaded {
            certificate: PathBuf::from("/etc/tls/fullchain.pem"),
        });

        assert_eq!(recorder.node.lock().len(), 1);
        assert!(recorder.events.lock().is_empty());
    }

    #[test]
    fn the_default_destination_discards() {
        let id = SessionId(nz::u64!(1));
        Events::default().scoped(id).emit(SessionEvent::Draining);
        Events::default().emit(NodeEvent::CertificateWatchLost {
            reason: "gone".into(),
        });
    }
}
