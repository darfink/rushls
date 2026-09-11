//! The small set of facts this node promises to anyone outside the process.
//!
//! [`SessionEvent`] is internal: fifteen variants, free to change with the
//! pipeline that emits them. What a webhook consumer subscribes to must not be.
//! So this is a *projection* rather than a rename — [`Projector::project`] is
//! the one place where an internal event becomes public, or deliberately does
//! not, and adding a variant upstream produces no external effect until
//! somebody decides here what it means.
//!
//! # Two lifetimes, not one
//!
//! A stream outlives any single publisher: a reconnect within
//! `inactive_stream_retention` ends one session and starts another while
//! viewers keep playing. Events are therefore named after which of the two they
//! describe, so that a consumer acting on `session.ended` cannot mistake an
//! encoder hiccup for a broadcast finishing.
//!
//! Every event carries the stream as its subject, so one ordered stream of
//! events per stream identity is enough to follow both lifetimes.

use std::collections::HashMap;

use derive_more::Display;
use parking_lot::Mutex;
use tokio::time::{Duration, Instant};

use crate::domain::{SessionId, StreamId};

use super::{SessionEnd, SessionEvent, StreamEvent};

/// Which fact an [`Event`] reports.
///
/// Spelled rather than derived, because these strings are what an operator
/// writes in a hook's subscription list and what a consumer routes on.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    /// A publisher was admitted and registered.
    #[display("session.started")]
    SessionStarted,
    /// The stream can be played.
    #[display("stream.available")]
    StreamAvailable,
    /// The stream can no longer be played.
    #[display("stream.unavailable")]
    StreamUnavailable,
    /// A publisher stopped, for any reason.
    #[display("session.ended")]
    SessionEnded,
}

impl Kind {
    /// Every kind, so a subscription list can be validated against one place.
    pub const ALL: [Self; 4] = [
        Self::SessionStarted,
        Self::StreamAvailable,
        Self::StreamUnavailable,
        Self::SessionEnded,
    ];
}

impl std::str::FromStr for Kind {
    type Err = UnknownKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.to_string() == value)
            .ok_or_else(|| UnknownKind(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown lifecycle event `{0}`")]
pub struct UnknownKind(pub String);

/// Why a publisher stopped.
///
/// Deliberately its own type rather than [`SessionEnd`], which it currently
/// mirrors exactly. These six names are a promise to consumers; the internal
/// enum is not, and the conversion below is where a future internal variant has
/// to be consciously given an external meaning instead of silently acquiring
/// one.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "lowercase")]
pub enum Outcome {
    /// The publisher's input ended.
    Ended,
    /// The input disappeared without a deliberate close.
    Interrupted,
    /// Another publisher took the stream over.
    Replaced,
    /// An operator or a shutdown stopped it.
    Cancelled,
    /// The session stopped making progress.
    Unhealthy,
    Failed,
}

impl From<SessionEnd> for Outcome {
    fn from(end: SessionEnd) -> Self {
        match end {
            SessionEnd::Ended => Self::Ended,
            SessionEnd::Interrupted => Self::Interrupted,
            SessionEnd::Replaced => Self::Replaced,
            SessionEnd::Cancelled => Self::Cancelled,
            SessionEnd::Unhealthy => Self::Unhealthy,
            SessionEnd::Failed => Self::Failed,
        }
    }
}

/// A publisher was admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStarted {
    pub stream: StreamId,
    pub session: SessionId,
    pub principal: String,
    pub publisher: crate::domain::PublisherContext,
}

/// The stream became playable.
///
/// Reported when the store first holds a presentation a viewer can resolve,
/// not when a publisher's pipeline starts running. Those differ, and the store
/// is the one that decides what a viewer can actually fetch.
///
/// Names no session. A stream can be made playable by one publisher and kept
/// playable across a reconnect by another, so attributing it to a session would
/// be picking one arbitrarily; the preceding `session.started` for this stream
/// is the publisher that did it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamAvailable {
    pub stream: StreamId,
}

/// The stream stopped being playable.
///
/// The end of the *stream's* life, not a publisher's: it fires when the
/// reconnect window closes and the store retires the stream, which is the
/// moment viewers begin getting 404s. A publisher disconnecting produces
/// `session.ended` and nothing more, because viewers keep playing.
///
/// Never emitted for a stream that was never playable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamUnavailable {
    pub stream: StreamId,
}

/// A publisher stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEnded {
    pub stream: StreamId,
    pub session: SessionId,
    pub principal: String,
    pub publisher: crate::domain::PublisherContext,
    pub outcome: Outcome,
    /// How long the publisher was admitted for.
    pub duration: Duration,
    /// Whether this publisher's pipeline ever started running.
    ///
    /// Tells a three-hour broadcast ending from an encoder that connected and
    /// immediately failed, without correlating against other events. About
    /// this publisher rather than about the stream: viewers may have been
    /// playing throughout on media an earlier publisher left behind.
    pub was_available: bool,
    /// Human-readable detail, for a person reading a log.
    ///
    /// Explicitly **not** a machine contract: the text comes from internal
    /// error types and changes with them. Anything a consumer branches on
    /// belongs in [`Self::outcome`].
    pub diagnostic: Option<String>,
}

/// One public fact about a stream or one of its publishers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    SessionStarted(SessionStarted),
    StreamAvailable(StreamAvailable),
    StreamUnavailable(StreamUnavailable),
    SessionEnded(SessionEnded),
}

impl Event {
    pub fn kind(&self) -> Kind {
        match self {
            Self::SessionStarted(_) => Kind::SessionStarted,
            Self::StreamAvailable(_) => Kind::StreamAvailable,
            Self::StreamUnavailable(_) => Kind::StreamUnavailable,
            Self::SessionEnded(_) => Kind::SessionEnded,
        }
    }

    /// The stream every event is about.
    ///
    /// Delivery orders events per subject, and this is that subject.
    pub fn stream(&self) -> &StreamId {
        match self {
            Self::SessionStarted(event) => &event.stream,
            Self::StreamAvailable(event) => &event.stream,
            Self::StreamUnavailable(event) => &event.stream,
            Self::SessionEnded(event) => &event.stream,
        }
    }

    /// The publisher an event is about, for the events that have one.
    ///
    /// `None` for stream-lifetime events, which is the distinction this module
    /// exists to keep.
    pub fn session(&self) -> Option<SessionId> {
        match self {
            Self::SessionStarted(event) => Some(event.session),
            Self::SessionEnded(event) => Some(event.session),
            Self::StreamAvailable(_) | Self::StreamUnavailable(_) => None,
        }
    }
}

/// Turns internal session events into public ones.
///
/// Stateful by necessity. Only [`SessionEvent::Accepted`] carries the stream
/// and principal, while `Running` and `Ended` carry neither, so identity has to
/// be remembered from admission until the session stops. The alternative —
/// widening the internal events to repeat identity — would shape them around an
/// external contract, which is the coupling this module exists to prevent.
///
/// Shared across every session task, so callers hold one and clone nothing.
#[derive(Debug, Default)]
pub struct Projector {
    live: Mutex<HashMap<SessionId, Tracked>>,
}

#[derive(Debug)]
struct Tracked {
    stream: StreamId,
    principal: String,
    publisher: crate::domain::PublisherContext,
    started_at: Instant,
    /// Whether this publisher's pipeline ever started running.
    reached_running: bool,
}

impl Projector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Projects a stream-lifetime fact, which belongs to no session.
    ///
    /// `None` when the fact is operator-facing but not a public lifecycle
    /// event: capacity clipping the playlist is logged, not delivered.
    pub fn project_stream(&self, stream: StreamId, event: StreamEvent) -> Option<Event> {
        match event {
            StreamEvent::Available => Some(Event::StreamAvailable(StreamAvailable { stream })),
            StreamEvent::Retired => Some(Event::StreamUnavailable(StreamUnavailable { stream })),
            StreamEvent::RetentionClipped { .. } => None,
        }
    }

    /// Projects one internal event, if it means anything externally.
    ///
    /// `None` for the majority: discovery, calibration, segmentation, and the
    /// rest describe how the pipeline works and would tie consumers to it.
    ///
    /// An event for a session that was never accepted is also `None`. That
    /// covers a publisher rejected during admission, which has no identity to
    /// report and no stream to report it against.
    pub fn project(&self, session: SessionId, event: &SessionEvent) -> Option<Event> {
        match event {
            SessionEvent::Accepted {
                stream,
                principal,
                publisher,
            } => {
                let mut live = self.live.lock();
                // A session id is unique per publisher, so an occupied entry
                // would mean the pipeline emitted `Accepted` twice.
                live.insert(
                    session,
                    Tracked {
                        stream: stream.clone(),
                        principal: principal.clone(),
                        publisher: publisher.clone(),
                        started_at: Instant::now(),
                        reached_running: false,
                    },
                );
                Some(Event::SessionStarted(SessionStarted {
                    stream: stream.clone(),
                    session,
                    principal: principal.clone(),
                    publisher: publisher.clone(),
                }))
            }
            SessionEvent::Running => {
                // Recorded but not reported. Whether *viewers* can play is the
                // store's answer, not the pipeline's, and it is reported
                // through [`Self::project_stream`]. All this remembers is
                // whether this publisher got that far, which is what
                // `session.ended` carries as `was_available`.
                self.live.lock().get_mut(&session)?.reached_running = true;
                None
            }
            SessionEvent::Ended { end } => self.finish(session, (*end).into(), None),
            SessionEvent::Failed { reason, .. } => {
                self.finish(session, Outcome::Failed, Some(reason.clone()))
            }
            // Everything else is pipeline detail. Listed as a catch-all rather
            // than variant by variant on purpose: a new internal event must be
            // added here deliberately to become public, and stays private until
            // someone decides what it would mean to a consumer.
            _ => None,
        }
    }

    /// Sessions admitted but not yet ended.
    ///
    /// Bounded by the node's session capacity in practice, because every
    /// session that reaches `Accepted` also reaches `Ended` or `Failed`.
    pub fn tracked(&self) -> usize {
        self.live.lock().len()
    }

    fn finish(
        &self,
        session: SessionId,
        outcome: Outcome,
        diagnostic: Option<String>,
    ) -> Option<Event> {
        let tracked = self.live.lock().remove(&session)?;
        Some(Event::SessionEnded(SessionEnded {
            stream: tracked.stream,
            session,
            principal: tracked.principal,
            publisher: tracked.publisher,
            outcome,
            duration: Instant::now().saturating_duration_since(tracked.started_at),
            was_available: tracked.reached_running,
            diagnostic,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TrackCounts;

    fn accepted(stream: &str) -> SessionEvent {
        SessionEvent::Accepted {
            stream: StreamId::new(stream),
            principal: "studio-camera".into(),
            publisher: crate::domain::fixtures::publisher(),
        }
    }

    #[test]
    fn a_publisher_reports_only_its_own_lifetime() {
        let projector = Projector::new();
        let session = SessionId(nz::u64!(1));

        let started = projector.project(session, &accepted("live/camera"));
        let running = projector.project(session, &SessionEvent::Running);
        let ended = projector.project(
            session,
            &SessionEvent::Ended {
                end: SessionEnd::Ended,
            },
        );

        assert!(matches!(started, Some(Event::SessionStarted(_))));
        assert!(
            running.is_none(),
            "a running pipeline is not the same fact as a playable stream: \
             what a viewer can fetch is the store's answer, and it arrives \
             through `project_stream`"
        );
        let Some(Event::SessionEnded(ended)) = ended else {
            panic!("a session that ran reports how it stopped");
        };
        assert_eq!(ended.stream, StreamId::new("live/camera"));
        assert_eq!(ended.principal, "studio-camera");
        assert_eq!(ended.outcome, Outcome::Ended);
        assert!(ended.was_available);
        assert_eq!(ended.diagnostic, None);
        assert_eq!(projector.tracked(), 0, "a finished session is forgotten");
    }

    #[test]
    fn a_publisher_that_never_ran_is_reported_as_never_available() {
        let projector = Projector::new();
        let session = SessionId(nz::u64!(7));

        projector.project(session, &accepted("live/camera"));
        let ended = projector.project(
            session,
            &SessionEvent::Failed {
                segmentation: None,
                reason: "the input delivered nothing for 5s".into(),
            },
        );

        let Some(Event::SessionEnded(ended)) = ended else {
            panic!("a failed session still reports an end");
        };
        assert_eq!(ended.outcome, Outcome::Failed);
        assert!(!ended.was_available);
        assert_eq!(
            ended.diagnostic.as_deref(),
            Some("the input delivered nothing for 5s"),
            "the internal message is carried for a human, not for branching"
        );
    }

    #[test]
    fn a_reconnect_reports_a_new_session_and_nothing_about_the_stream() {
        let projector = Projector::new();
        let (first, second) = (SessionId(nz::u64!(1)), SessionId(nz::u64!(2)));

        projector.project(first, &accepted("live/camera"));
        projector.project(first, &SessionEvent::Running);
        projector.project(
            first,
            &SessionEvent::Ended {
                end: SessionEnd::Interrupted,
            },
        );

        let restarted = projector.project(second, &accepted("live/camera"));
        let running = projector.project(second, &SessionEvent::Running);

        assert!(matches!(restarted, Some(Event::SessionStarted(_))));
        assert!(
            running.is_none(),
            "viewers never lost the stream, so nothing about the stream \
             changed; only the publisher did"
        );
    }

    #[test]
    fn the_two_lifetimes_are_reported_by_different_paths() {
        let projector = Projector::new();
        let stream = StreamId::new("live/camera");

        let available = projector
            .project_stream(stream.clone(), StreamEvent::Available)
            .expect("availability is public");
        let retired = projector
            .project_stream(stream.clone(), StreamEvent::Retired)
            .expect("retirement is public");

        assert_eq!(available.kind(), Kind::StreamAvailable);
        assert_eq!(retired.kind(), Kind::StreamUnavailable);
        assert_eq!(available.stream(), &stream);
        assert_eq!(
            (available.session(), retired.session()),
            (None, None),
            "a stream can be made playable by one publisher and kept playable \
             by the next, so naming one would be picking arbitrarily"
        );
        assert!(
            projector
                .project_stream(
                    stream,
                    StreamEvent::RetentionClipped {
                        reason: crate::observe::RetentionClipReason::Memory,
                        requested: Duration::from_mins(15),
                        held: Duration::from_secs(18),
                    },
                )
                .is_none(),
            "capacity clipping is logged, not a public lifecycle event"
        );
    }

    #[test]
    fn pipeline_detail_stays_internal() {
        let projector = Projector::new();
        let session = SessionId(nz::u64!(1));
        projector.project(session, &accepted("live/camera"));

        for event in [
            SessionEvent::TracksDiscovered {
                counts: TrackCounts::default(),
            },
            SessionEvent::SegmentationLocked {
                segment: Duration::from_secs(6),
                part: Duration::from_secs(1),
            },
            SessionEvent::Draining,
            SessionEvent::TrackSetChanged,
        ] {
            assert_eq!(projector.project(session, &event), None);
        }
    }

    #[test]
    fn events_for_an_unknown_session_are_dropped_rather_than_invented() {
        let projector = Projector::new();

        assert_eq!(
            projector.project(SessionId(nz::u64!(9)), &SessionEvent::Running),
            None
        );
        assert_eq!(
            projector.project(
                SessionId(nz::u64!(9)),
                &SessionEvent::Ended {
                    end: SessionEnd::Ended
                }
            ),
            None
        );
        assert_eq!(projector.tracked(), 0);
    }

    #[test]
    fn subscription_names_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(kind.to_string().parse(), Ok(kind));
        }
        assert_eq!(
            "session.paused".parse::<Kind>(),
            Err(UnknownKind("session.paused".into())),
            "a name this node does not emit is refused rather than ignored, so \
             a typo in a subscription is a startup error and not a hook that \
             silently never fires"
        );
    }
}
