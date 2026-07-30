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

use super::{SessionEnd, SessionEvent};

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
    /// A publisher stopped, for any reason.
    #[display("session.ended")]
    SessionEnded,
}

impl Kind {
    /// Every kind, so a subscription list can be validated against one place.
    pub const ALL: [Self; 3] = [
        Self::SessionStarted,
        Self::StreamAvailable,
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
}

/// The stream became playable.
///
/// Currently the instant the pipeline starts running, which is when a muxer and
/// publisher exist and media begins reaching the store. A viewer's very first
/// request may still block briefly for `playlist_readiness`; nothing here
/// promises otherwise, and narrowing this to the first satisfying store write
/// would not change what the event means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamAvailable {
    pub stream: StreamId,
    /// The publisher that made it playable.
    pub session: SessionId,
}

/// A publisher stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEnded {
    pub stream: StreamId,
    pub session: SessionId,
    pub principal: String,
    pub outcome: Outcome,
    /// How long the publisher was admitted for.
    pub duration: Duration,
    /// Whether this publisher ever made the stream playable.
    ///
    /// Saves a consumer from correlating against an earlier
    /// [`StreamAvailable`] to tell a three-hour broadcast ending from an
    /// encoder that connected and immediately failed.
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
    SessionEnded(SessionEnded),
}

impl Event {
    pub fn kind(&self) -> Kind {
        match self {
            Self::SessionStarted(_) => Kind::SessionStarted,
            Self::StreamAvailable(_) => Kind::StreamAvailable,
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
            Self::SessionEnded(event) => &event.stream,
        }
    }

    pub fn session(&self) -> SessionId {
        match self {
            Self::SessionStarted(event) => event.session,
            Self::StreamAvailable(event) => event.session,
            Self::SessionEnded(event) => event.session,
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
    started_at: Instant,
    available: bool,
}

impl Projector {
    pub fn new() -> Self {
        Self::default()
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
            SessionEvent::Accepted { stream, principal } => {
                let mut live = self.live.lock();
                // A session id is unique per publisher, so an occupied entry
                // would mean the pipeline emitted `Accepted` twice.
                live.insert(
                    session,
                    Tracked {
                        stream: stream.clone(),
                        principal: principal.clone(),
                        started_at: Instant::now(),
                        available: false,
                    },
                );
                Some(Event::SessionStarted(SessionStarted {
                    stream: stream.clone(),
                    session,
                    principal: principal.clone(),
                }))
            }
            SessionEvent::Running => {
                let mut live = self.live.lock();
                let tracked = live.get_mut(&session)?;
                // Latched: `Running` is entered once per session today, and a
                // consumer that saw the stream come up must not be told again.
                if tracked.available {
                    return None;
                }
                tracked.available = true;
                Some(Event::StreamAvailable(StreamAvailable {
                    stream: tracked.stream.clone(),
                    session,
                }))
            }
            SessionEvent::Ended { end } => self.finish(session, (*end).into(), None),
            SessionEvent::Failed { reason } => {
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
            outcome,
            duration: Instant::now().saturating_duration_since(tracked.started_at),
            was_available: tracked.available,
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
        }
    }

    #[test]
    fn a_publisher_produces_a_start_an_availability_and_an_end() {
        let projector = Projector::new();
        let session = SessionId(nz::u64!(1));

        let started = projector.project(session, &accepted("live/camera"));
        let available = projector.project(session, &SessionEvent::Running);
        let ended = projector.project(
            session,
            &SessionEvent::Ended {
                end: SessionEnd::Ended,
            },
        );

        assert!(matches!(started, Some(Event::SessionStarted(_))));
        assert!(matches!(available, Some(Event::StreamAvailable(_))));
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
    fn a_reconnect_starts_a_new_session_without_repeating_availability() {
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

        // The stream stayed playable across the gap, so the second publisher
        // reports its own session but the same stream never "became" available
        // to a consumer that was already told it was.
        let restarted = projector.project(second, &accepted("live/camera"));
        let available = projector.project(second, &SessionEvent::Running);

        assert!(matches!(restarted, Some(Event::SessionStarted(_))));
        assert_eq!(
            available.map(|event| event.kind()),
            Some(Kind::StreamAvailable),
            "availability is latched per publisher; de-duplicating across a \
             reconnect gap needs the store's retirement, which is not wired yet"
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
            "stream.unavailable".parse::<Kind>(),
            Err(UnknownKind("stream.unavailable".into())),
            "a name this node does not emit is refused rather than ignored"
        );
    }
}
