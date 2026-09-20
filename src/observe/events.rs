use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use derive_more::{Debug, Display};

use crate::domain::{SessionId, StreamId, TrackCounts, TrackId};

use super::lifecycle;

/// A rare, structured fact about a session's progress through its lifecycle.
///
/// Payloads stay primitive so this layer never depends on a domain aggregate.
/// Anything that happens per packet, sample, or part belongs in
/// [`SessionMeters`](super::SessionMeters) instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Compensation {
        notice: crate::domain::NormalizationNotice,
    },
    Accepted {
        stream: StreamId,
        principal: String,
        publisher: crate::domain::PublisherContext,
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
    SegmentationContract {
        desired_segment: Duration,
        desired_part: Duration,
        selected_segment: Duration,
        selected_part: Duration,
        maximum_segment: Duration,
        maximum_part: Duration,
        jitter: Duration,
    },
    /// A non-strict muxer kept parts flowing while extending a segment to a
    /// usable random-access boundary.
    SegmentationExtended {
        track: TrackId,
        planned: Duration,
        actual: Duration,
    },
    /// A subtitle cue arrived after every segment covering it was published.
    ///
    /// Sparse subtitle renditions are sealed on the presentation clock so their
    /// playlist keeps pace with its siblings, which means a cue can miss its
    /// window entirely. Nothing can carry it once that happens, so it is
    /// dropped. Named per track and per cue because the interesting question is
    /// which input runs late and by how much, which a counter cannot answer.
    SubtitleCueTooLate {
        track: TrackId,
        late_by: Duration,
    },
    /// One unchanged subtitle display state remained active unusually long.
    ///
    /// This is diagnostic only: an open-ended caption is publisher-owned and
    /// remains visible until an explicit replacement or clear. Reporting once
    /// per state generation makes a lost clear observable without introducing
    /// a second, consumer-side presentation timeout.
    SubtitleStateLongLived {
        track: TrackId,
        started_at: Duration,
        age: Duration,
    },
    /// In-band closed captions were detected and are now advertised.
    ///
    /// Carries the channels so an operator can tell a 608-only publisher from a
    /// 708 one without inspecting the playlist, which is the distinction that
    /// decides whether browser clients will surface the captions at all.
    ClosedCaptionsDetected {
        channels: Vec<String>,
    },
    /// Captions were seen on some video tracks but not all of them.
    ///
    /// Nothing is declared while this holds: HLS advertises captions once for
    /// the presentation, so a client that switched to a rendition without them
    /// would silently lose the captions mid-playback. Reported per publication
    /// because it is a fault in what was published, not something this node can
    /// correct.
    ClosedCaptionsPartial {
        carrying: usize,
        video_tracks: usize,
    },
    /// Every video track carries captions, but not on the same channels.
    ///
    /// Nothing is declared: a channel missing from one rendition cannot be
    /// promised for the presentation. Separate from
    /// [`Self::ClosedCaptionsPartial`] because the remedy differs — the ladder
    /// is captioned throughout, just not uniformly.
    ClosedCaptionsChannelMismatch,
    /// A publisher emitted SEI this node could not parse.
    ///
    /// Counted per publication rather than per message: a stream with broken
    /// SEI usually has it in every access unit, and the interesting fact is
    /// that captions may be under-reported, not how many times.
    ClosedCaptionsMalformedSei {
        messages: u64,
    },
    Running,
    TrackSetChanged,
    CodecParametersChanged {
        track: TrackId,
    },
    /// Media time is advancing slower than wall clock.
    ///
    /// Diagnostic only: a slow publisher is ended by `floor` or by nothing at
    /// all, and inventing a second threshold with teeth is exactly what the
    /// deleted publication deadline did. This exists because an operator who
    /// set no floor still wants to know their live stream is not live, and a
    /// scraped gauge answers that only for whoever is watching the graph.
    ///
    /// Reported once per transition rather than once per window, so a stream
    /// that stays behind for an hour logs twice: here, and again at
    /// [`Self::PublisherTrackingRealtime`] when it recovers.
    PublisherBehindRealtime {
        /// The window judged, which is wall clock rather than media time.
        window: Duration,
        /// Media advanced across it. Less than `window`, by definition.
        media: Duration,
    },
    /// Media time is keeping up with wall clock again.
    PublisherTrackingRealtime,
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
        timestamp_issue: Option<Box<crate::domain::TimestampIssue>>,
        reason: String,
        segmentation: Option<crate::mux::MuxError>,
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
    /// The process received its shutdown request and is beginning graceful
    /// termination.
    ShuttingDown,
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
    /// A lifecycle event reached its last attempt without being delivered.
    ///
    /// Carries the event id so an operator can match a consumer's complaint
    /// against what this node believed it sent. Never the body: that names
    /// streams and principals, and a log has a far wider audience than the one
    /// endpoint the event was addressed to.
    HookEventDropped {
        hook: Arc<str>,
        /// The CloudEvents `id`, which is what the consumer would quote.
        event: String,
        kind: lifecycle::Kind,
        /// Why it will not be attempted again.
        reason: &'static str,
        detail: String,
    },
    /// Events still queued when the drain deadline passed.
    HookEventsAbandoned { hook: Arc<str>, dropped: usize },
    /// Delivery was active when shutdown's drain deadline passed.
    ///
    /// Unlike a queued event, this may have reached the endpoint before the
    /// request was cancelled, so its outcome is unknown rather than dropped.
    HookDeliveryOutcomesUnknown { hook: Arc<str>, count: usize },
    /// An event could not be turned into bytes at all.
    ///
    /// A fault in this process rather than in delivery, so it names no hook:
    /// nothing was addressed yet when it failed.
    HookEventUnrenderable { reason: String },
    /// The recorder started losing archive segments; live delivery continues.
    ///
    /// A transition, not a per-segment report: it fires once when a healthy
    /// recorder begins failing and stays quiet while the failure persists, so a
    /// stalled disk is one event rather than one per segment. The rate belongs
    /// in [`ProcessMeters`](super::ProcessMeters); [`Self::RecordingRecovered`]
    /// closes the window and carries the total lost while it was open.
    RecordingFailed { stream: StreamId, reason: String },
    /// Archive writes are succeeding again after [`Self::RecordingFailed`].
    ///
    /// Carries the segments lost while the window was open, so a consumer that
    /// saw only the edge can still account for everything it thereby missed.
    /// The window is node-wide rather than per-stream because every cause —
    /// disk, delivery queue, and byte budget — is a resource shared by all
    /// streams on the node.
    RecordingRecovered { lost: usize },
    /// These writes may still complete, but shutdown no longer waits for them.
    RecordingDrainExpired { pending: usize },
    /// A connection ended before it said what it wanted to publish.
    ///
    /// How often this happens is up to whoever is connecting, so the count
    /// belongs in [`ProcessMeters`](super::ProcessMeters); this carries the
    /// reason, which a counter cannot.
    PublisherHandshakeFailed { protocol: Protocol, reason: String },
    /// An admitted publication ended in an error.
    ///
    /// Overlaps [`SessionEvent::Failed`] for a session that got as far as
    /// registering, and is the only report for one that did not — admission
    /// runs before a session context exists. Narrowing that belongs with the
    /// revision `SessionEvent` is already due.
    PublisherSessionFailed { protocol: Protocol, reason: String },
    /// A task running one connection panicked.
    ///
    /// Always a defect: a session reports its own failures through
    /// [`SessionEvent`], so reaching this means one did not get the chance.
    ConnectionTaskPanicked { protocol: Protocol, reason: String },
    /// A listener is accepting but could not say where.
    ///
    /// Only interesting because it makes an ephemeral port unknowable, which
    /// is the one case where nothing else can report the address.
    ListenerAddressUnavailable { protocol: Protocol, reason: String },
    /// A listener could not accept, and will retry shortly.
    ///
    /// Local rather than remote: the peer is gone by definition, so this is
    /// descriptor exhaustion or something like it.
    ListenerAcceptFailed { protocol: Protocol, reason: String },
}

/// Which listener an event is about.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "lowercase")]
pub enum Protocol {
    Rtmp,
    Srt,
    Http,
    Https,
    Moq,
}

pub trait EventObserver: Send + Sync {
    fn observe(&self, session: SessionId, event: SessionEvent);

    /// A fact about a stream rather than about one of its publishers.
    ///
    /// Separate from [`Self::observe`] because a stream outlives any single
    /// session: a publisher reconnecting within the idle window ends one
    /// session and starts another while viewers keep playing throughout. An
    /// observer that conflated the two would tell a consumer the broadcast
    /// stopped every time an encoder hiccuped.
    ///
    /// Defaulted, like [`Self::observe_node`], so an observer that only cares
    /// about sessions stays a one-method impl.
    fn observe_stream(&self, _stream: StreamId, _event: StreamEvent) {}

    /// Defaulted so an observer that only cares about sessions stays a
    /// one-method impl.
    fn observe_node(&self, _event: NodeEvent) {}
}

/// What changed about a stream's own lifetime, rather than one publisher's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    /// A completed, non-GAP segment was committed to delivery.
    SegmentReady(super::lifecycle::ReadySegment),
    /// The store has a presentation a viewer can play.
    Available,
    /// The reconnect window closed, so viewers can no longer reach it.
    ///
    /// Only reported for a stream that was playable: one retired without ever
    /// serving anything never became unavailable, because it never was.
    Retired,
    /// Oldest playable media was dropped because a byte or object cap is full.
    ///
    /// Sliding off `retain` is silent. This fires once when a cap first
    /// shortens the playlist, and again only if the playlist later fills
    /// `retain` and a cap drops media again.
    RetentionClipped {
        reason: RetentionClipReason,
        requested: Duration,
        held: Duration,
    },
}

/// The storage limit that forced media to be dropped.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "lowercase")]
pub enum RetentionClipReason {
    Memory,
    Disk,
    #[display("part or segment count")]
    Objects,
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

    /// Reports a fact about a stream, which outlives any one publisher.
    pub fn stream(&self, stream: StreamId, event: StreamEvent) {
        self.0.observe_stream(stream, event);
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
