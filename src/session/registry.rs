use std::{
    collections::HashMap,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use parking_lot::RwLock;
use thiserror::Error;

use crate::{
    admission::{Principal, PublishGrant, TakeoverPolicy},
    domain::{SessionId, StreamId, TrackCounts},
    observe::{Events, MeterSnapshot, SessionMeters},
};

use super::{Phase, SessionContext, StopReason, StopToken};

/// A point-in-time view of one running session.
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub stream: StreamId,
    pub principal: Principal,
    pub phase: Phase,
    pub tracks: TrackCounts,
    pub meters: MeterSnapshot,
    pub track_progress: Vec<crate::observe::tracks::TrackSnapshot>,
}

/// Identity and mutable status shared between a session and its observers.
#[derive(Debug)]
pub struct SessionShared {
    id: SessionId,
    stream: StreamId,
    principal: Principal,
    meters: SessionMeters,
    status: RwLock<Status>,
}

#[derive(Clone, Copy, Debug)]
struct Status {
    phase: Phase,
    tracks: TrackCounts,
}

impl SessionShared {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn meters(&self) -> &SessionMeters {
        &self.meters
    }

    pub fn phase(&self) -> Phase {
        self.status.read().phase
    }

    pub(super) fn set_phase(&self, phase: Phase) {
        self.status.write().phase = phase;
    }

    pub(super) fn set_tracks(&self, tracks: TrackCounts) {
        self.status.write().tracks = tracks;
    }

    fn snapshot(&self) -> SessionSnapshot {
        let status = *self.status.read();
        SessionSnapshot {
            id: self.id,
            stream: self.stream.clone(),
            principal: self.principal.clone(),
            phase: status.phase,
            tracks: status.tracks,
            meters: self.meters.snapshot(),
            track_progress: self.meters.tracks().snapshot(),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("this node is already running its maximum of {maximum} sessions")]
pub struct AtCapacity {
    pub maximum: usize,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RegistryError {
    #[error(transparent)]
    AtCapacity(#[from] AtCapacity),
    #[error("another publisher already owns {stream}")]
    AlreadyPublished { stream: StreamId },
    #[error("a previous publisher of {stream} is still draining")]
    TakeoverInProgress { stream: StreamId },
}

/// Every session currently running in this process.
#[derive(Clone, Debug)]
pub struct Registry {
    inner: Arc<Inner>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MAXIMUM_SESSIONS)
    }
}

/// Concurrent publications a node accepts before turning publishers away.
///
/// A ceiling has to exist somewhere: every session holds buffers, a delivery
/// window, and a task, so unbounded admission turns a burst of connections into
/// memory exhaustion. The value is a placeholder for whatever capacity planning
/// says; what matters is that the limit is enforced rather than implied.
const DEFAULT_MAXIMUM_SESSIONS: usize = 256;

#[derive(Debug)]
struct Inner {
    next_id: AtomicU64,
    maximum: usize,
    sessions: RwLock<HashMap<SessionId, Entry>>,
}

#[derive(Debug)]
struct Entry {
    shared: Arc<SessionShared>,
    stop: StopToken,
}

impl Registry {
    pub fn with_capacity(maximum: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(0),
                maximum,
                sessions: RwLock::new(HashMap::new()),
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.inner.maximum
    }

    /// Checks whether a grant could be registered right now.
    ///
    /// Advisory: the answer can go stale before [`Self::register`] is called,
    /// which is why that enforces the limit too. This exists so a publisher can
    /// be turned away with the correct protocol-level reason instead of having
    /// its handshake accepted and then immediately dropped.
    pub fn preflight(&self, grant: &PublishGrant) -> Result<(), RegistryError> {
        let sessions = self.inner.sessions.read();
        self.check(&sessions, grant)
    }

    /// Admits a session and displaces any incumbent holding the same stream.
    ///
    /// Takeover is resolved here rather than at admission because this is the
    /// only place that knows what is currently running. The authenticated
    /// [`TakeoverPolicy`] decides whether an incumbent may be displaced.
    ///
    /// An allowed takeover is admitted even at capacity: it replaces an active
    /// publication, though the displaced session remains registered for its
    /// bounded drain. Further takeovers wait for that drain to finish.
    pub fn register(
        &self,
        grant: &PublishGrant,
        meters: SessionMeters,
        stop: StopToken,
    ) -> Result<Registration, RegistryError> {
        let mut sessions = self.inner.sessions.write();
        self.check(&sessions, grant)?;
        let displaced: Vec<_> = sessions
            .values()
            .filter(|entry| entry.shared.stream == grant.stream_id)
            .map(|entry| entry.stop.clone())
            .collect();

        let raw_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let id = SessionId(NonZeroU64::new(raw_id).expect("session id counter wrapped"));
        let shared = Arc::new(SessionShared {
            id,
            stream: grant.stream_id.clone(),
            principal: grant.principal.clone(),
            meters,
            status: RwLock::new(Status {
                phase: Phase::Accepted,
                tracks: TrackCounts::default(),
            }),
        });
        sessions.insert(
            id,
            Entry {
                shared: Arc::clone(&shared),
                stop,
            },
        );
        drop(sessions);

        let displaced_count = displaced.len();
        for stop in displaced {
            stop.stop(StopReason::Replaced);
        }

        Ok(Registration {
            registry: self.clone(),
            shared,
            displaced: displaced_count,
        })
    }

    fn check(
        &self,
        sessions: &HashMap<SessionId, Entry>,
        grant: &PublishGrant,
    ) -> Result<(), RegistryError> {
        let mut incumbent = false;
        let mut draining = false;
        for entry in sessions
            .values()
            .filter(|entry| entry.shared.stream == grant.stream_id)
        {
            incumbent = true;
            draining |= entry.stop.reason().is_some();
        }

        if incumbent && grant.policy.takeovers == TakeoverPolicy::Deny {
            return Err(RegistryError::AlreadyPublished {
                stream: grant.stream_id.clone(),
            });
        }
        if draining {
            return Err(RegistryError::TakeoverInProgress {
                stream: grant.stream_id.clone(),
            });
        }
        if !incumbent && sessions.len() >= self.inner.maximum {
            return Err(AtCapacity {
                maximum: self.inner.maximum,
            }
            .into());
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        self.inner
            .sessions
            .read()
            .values()
            .map(|entry| entry.shared.snapshot())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.sessions.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.sessions.read().is_empty()
    }

    /// Asks every running session to stop. Used for graceful shutdown.
    pub fn stop_all(&self, reason: StopReason) {
        for entry in self.inner.sessions.read().values() {
            entry.stop.stop(reason);
        }
    }
}

/// A session's presence in the registry, withdrawn when dropped.
#[derive(Debug)]
pub struct Registration {
    registry: Registry,
    shared: Arc<SessionShared>,
    displaced: usize,
}

impl Registration {
    pub fn id(&self) -> SessionId {
        self.shared.id
    }

    pub fn shared(&self) -> &Arc<SessionShared> {
        &self.shared
    }

    /// How many incumbent sessions this registration displaced.
    pub fn displaced(&self) -> usize {
        self.displaced
    }

    pub fn context(&self, events: &Events, stop: StopToken) -> SessionContext {
        SessionContext::new(
            Arc::clone(&self.shared),
            events.scoped(self.shared.id),
            stop,
        )
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.inner.sessions.write().remove(&self.shared.id);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::{StreamPolicy, TakeoverPolicy},
        observe::ProcessMeters,
    };

    use super::*;

    fn grant(stream: &str) -> PublishGrant {
        PublishGrant {
            stream_id: StreamId::new(stream),
            principal: Principal("publisher".into()),
            policy: StreamPolicy::permissive(),
        }
    }

    fn grant_with_takeovers(stream: &str, takeovers: TakeoverPolicy) -> PublishGrant {
        PublishGrant {
            policy: StreamPolicy {
                takeovers,
                ..StreamPolicy::permissive()
            },
            ..grant(stream)
        }
    }

    fn try_register(
        registry: &Registry,
        stream: &str,
        stop: StopToken,
    ) -> Result<Registration, RegistryError> {
        registry.register(
            &grant(stream),
            SessionMeters::new(ProcessMeters::default()),
            stop,
        )
    }

    fn register(registry: &Registry, stream: &str, stop: StopToken) -> Registration {
        try_register(registry, stream, stop).expect("the registry has room")
    }

    #[test]
    fn registration_lifetime_bounds_visibility() {
        let registry = Registry::default();
        let registration = register(&registry, "live/camera", StopToken::new());
        registration.shared().set_phase(Phase::Running);
        registration.shared().set_tracks(TrackCounts {
            audio: 1,
            subtitle: 0,
            video: 2,
        });

        let snapshots = registry.snapshot();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].phase, Phase::Running);
        assert_eq!(snapshots[0].tracks.video, 2);

        drop(registration);
        assert!(registry.is_empty());
    }

    #[test]
    fn a_second_publisher_of_a_stream_displaces_the_first() {
        let registry = Registry::default();
        let incumbent_stop = StopToken::new();
        let _incumbent = register(&registry, "live/camera", incumbent_stop.clone());

        let takeover = register(&registry, "live/camera", StopToken::new());

        assert_eq!(takeover.displaced(), 1);
        assert_eq!(incumbent_stop.reason(), Some(StopReason::Replaced));
    }

    #[test]
    fn publishers_of_different_streams_coexist() {
        let registry = Registry::default();
        let first_stop = StopToken::new();
        let _first = register(&registry, "live/one", first_stop.clone());

        let second = register(&registry, "live/two", StopToken::new());

        assert_eq!(second.displaced(), 0);
        assert_eq!(first_stop.reason(), None);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn a_full_registry_turns_new_publishers_away() {
        let registry = Registry::with_capacity(1);
        let _held = register(&registry, "live/one", StopToken::new());

        assert_eq!(
            registry.preflight(&grant("live/two")),
            Err(AtCapacity { maximum: 1 }.into())
        );
        assert_eq!(
            try_register(&registry, "live/two", StopToken::new()).map(|_| ()),
            Err(AtCapacity { maximum: 1 }.into())
        );
    }

    #[test]
    fn capacity_never_blocks_a_publisher_reclaiming_its_own_stream() {
        let registry = Registry::with_capacity(1);
        let incumbent_stop = StopToken::new();
        let _incumbent = register(&registry, "live/one", incumbent_stop.clone());

        assert_eq!(registry.preflight(&grant("live/one")), Ok(()));
        let takeover = register(&registry, "live/one", StopToken::new());

        assert_eq!(takeover.displaced(), 1);
        assert_eq!(incumbent_stop.reason(), Some(StopReason::Replaced));
    }

    #[test]
    fn only_one_takeover_may_wait_for_a_stream_to_drain() {
        let registry = Registry::with_capacity(1);
        let incumbent_stop = StopToken::new();
        let _incumbent = register(&registry, "live/one", incumbent_stop);
        let _takeover = register(&registry, "live/one", StopToken::new());

        assert_eq!(
            registry.preflight(&grant("live/one")),
            Err(RegistryError::TakeoverInProgress {
                stream: StreamId::new("live/one"),
            })
        );
        assert_eq!(
            try_register(&registry, "live/one", StopToken::new()).map(|_| ()),
            Err(RegistryError::TakeoverInProgress {
                stream: StreamId::new("live/one"),
            })
        );
    }

    #[test]
    fn a_policy_can_protect_the_incumbent_from_takeover() {
        let registry = Registry::default();
        let incumbent_stop = StopToken::new();
        let _incumbent = register(&registry, "live/one", incumbent_stop.clone());
        let denied = grant_with_takeovers("live/one", TakeoverPolicy::Deny);

        assert_eq!(
            registry.preflight(&denied),
            Err(RegistryError::AlreadyPublished {
                stream: StreamId::new("live/one"),
            })
        );
        assert_eq!(
            registry
                .register(
                    &denied,
                    SessionMeters::new(ProcessMeters::default()),
                    StopToken::new(),
                )
                .map(|_| ()),
            Err(RegistryError::AlreadyPublished {
                stream: StreamId::new("live/one"),
            })
        );
        assert_eq!(incumbent_stop.reason(), None);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn shutdown_stops_every_session() {
        let registry = Registry::default();
        let first_stop = StopToken::new();
        let second_stop = StopToken::new();
        let _first = register(&registry, "live/one", first_stop.clone());
        let _second = register(&registry, "live/two", second_stop.clone());

        registry.stop_all(StopReason::Cancelled);

        assert_eq!(first_stop.reason(), Some(StopReason::Cancelled));
        assert_eq!(second_stop.reason(), Some(StopReason::Cancelled));
    }
}
