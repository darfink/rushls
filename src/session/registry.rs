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
    delivery::store::{LiveStream, StreamStore},
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
    pub(super) stop: StopToken,
    pub(super) public_stream: bool,
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
    #[error(transparent)]
    MemoryFull(#[from] crate::domain::MemoryFull),
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
    /// `memory.total`, and what each session commits against it.
    memory: Option<(crate::domain::MemoryLedger, usize)>,
    sessions: RwLock<HashMap<SessionId, Arc<SessionShared>>>,
}

impl Registry {
    pub fn with_capacity(maximum: usize) -> Self {
        Self::with_memory(maximum, None)
    }

    /// As [`Self::with_capacity`], also committing `per_publisher` bytes of a
    /// node-wide ledger for every registered session.
    pub fn with_memory(
        maximum: usize,
        memory: Option<(crate::domain::MemoryLedger, usize)>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(0),
                maximum,
                memory,
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
        self.check(&sessions, grant).map(|_| ())
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
        store: &StreamStore,
        public_stream: bool,
    ) -> Result<Registration, RegistryError> {
        store.with_retained_stream(&grant.stream_id, |retained| {
            self.register_publisher(grant, meters, public_stream, retained)
        })
    }

    fn register_publisher(
        &self,
        grant: &PublishGrant,
        meters: SessionMeters,
        mut public_stream: bool,
        retained: Option<&LiveStream>,
    ) -> Result<Registration, RegistryError> {
        let mut sessions = self.inner.sessions.write();
        let takeover = self.check(&sessions, grant)?;
        // Under the write lock, so a check and its commit cannot interleave
        // with another registration's.
        let memory = self
            .inner
            .memory
            .as_ref()
            .map(|(ledger, bytes)| {
                if takeover {
                    Ok(ledger.commit_over(*bytes))
                } else {
                    ledger.commit(*bytes)
                }
            })
            .transpose()?;
        // Publish the stop before another registration can observe the new owner.
        let mut displaced = 0;
        for entry in sessions
            .values()
            .filter(|entry| entry.stream == grant.stream_id)
        {
            public_stream &= entry.public_stream;
            entry.stop.stop(StopReason::Replaced);
            displaced += 1;
        }

        let raw_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let id = SessionId(NonZeroU64::new(raw_id).expect("session id counter wrapped"));
        let shared = Arc::new(SessionShared {
            id,
            stream: grant.stream_id.clone(),
            principal: grant.principal.clone(),
            stop: StopToken::new(),
            // Inherit privacy even if the previous publisher is still discovering
            // or has already left a retained stream in its reconnect window.
            public_stream: retained
                .map_or(public_stream, |live| live.restrict_hooks(public_stream)),
            meters,
            status: RwLock::new(Status {
                phase: Phase::Accepted,
                tracks: TrackCounts::default(),
            }),
        });
        sessions.insert(id, Arc::clone(&shared));
        Ok(Registration {
            registry: self.clone(),
            shared,
            displaced,
            _memory: memory,
        })
    }

    /// Returns whether admitting `grant` is a takeover of a running session.
    fn check(
        &self,
        sessions: &HashMap<SessionId, Arc<SessionShared>>,
        grant: &PublishGrant,
    ) -> Result<bool, RegistryError> {
        let mut incumbent = false;
        let mut draining = false;
        for entry in sessions
            .values()
            .filter(|entry| entry.stream == grant.stream_id)
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
        // A takeover replaces a session that already holds its commitment,
        // so only a new publication has to fit.
        if !incumbent
            && let Some((ledger, bytes)) = &self.inner.memory
            && ledger.committed().saturating_add(*bytes) > ledger.limit()
        {
            return Err(crate::domain::MemoryFull {
                requested: *bytes,
                committed: ledger.committed(),
                limit: ledger.limit(),
            }
            .into());
        }
        Ok(incumbent)
    }

    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        self.inner
            .sessions
            .read()
            .values()
            .map(|entry| entry.snapshot())
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
    /// This session's share of `memory.total`, returned with the registration.
    _memory: Option<crate::domain::Commitment>,
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

    pub fn context(&self, events: &Events) -> SessionContext {
        SessionContext::new(Arc::clone(&self.shared), events.scoped(self.shared.id))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.inner.sessions.write().remove(&self.shared.id);
    }
}

#[cfg(test)]
mod tests {
    use crate::{admission::StreamPolicy, observe::ProcessMeters};

    use super::*;

    fn grant(stream: &str) -> PublishGrant {
        PublishGrant {
            stream_id: StreamId::new(stream),
            principal: Principal("publisher".into()),
            policy: StreamPolicy::permissive(),
        }
    }

    fn try_register(
        registry: &Registry,
        grant: &PublishGrant,
    ) -> Result<Registration, RegistryError> {
        registry.register(
            grant,
            SessionMeters::new(ProcessMeters::default()),
            &StreamStore::default(),
            true,
        )
    }

    fn register(registry: &Registry, stream: &str) -> Result<Registration, RegistryError> {
        try_register(registry, &grant(stream))
    }

    fn register_with_stop(
        registry: &Registry,
        stream: &str,
    ) -> Result<(Registration, StopToken), RegistryError> {
        let registration = register(registry, stream)?;
        let stop = registration.shared.stop.clone();
        Ok((registration, stop))
    }

    fn assert_registration_rejected(
        registry: &Registry,
        grant: &PublishGrant,
        error: RegistryError,
    ) {
        assert_eq!(
            registry.preflight(grant),
            Err(error.clone()),
            "preflight for {}",
            grant.stream_id,
        );
        assert_eq!(
            try_register(registry, grant).map(|_| ()),
            Err(error),
            "registration for {}",
            grant.stream_id,
        );
    }

    #[test]
    fn registration_lifetime_bounds_visibility() -> Result<(), RegistryError> {
        let registry = Registry::default();
        let registration = register(&registry, "live/camera")?;
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
        Ok(())
    }

    #[test]
    fn a_second_publisher_of_a_stream_displaces_the_first() -> Result<(), RegistryError> {
        let registry = Registry::default();
        let (_incumbent, incumbent_stop) = register_with_stop(&registry, "live/camera")?;

        let takeover = register(&registry, "live/camera")?;

        assert_eq!(takeover.displaced(), 1);
        assert_eq!(incumbent_stop.reason(), Some(StopReason::Replaced));
        Ok(())
    }

    #[test]
    fn publishers_of_different_streams_coexist() -> Result<(), RegistryError> {
        let registry = Registry::default();
        let (_first, first_stop) = register_with_stop(&registry, "live/one")?;

        let second = register(&registry, "live/two")?;

        assert_eq!(second.displaced(), 0);
        assert_eq!(first_stop.reason(), None);
        assert_eq!(registry.len(), 2);
        Ok(())
    }

    #[test]
    fn sessions_commit_their_budget_against_the_node_total() -> Result<(), RegistryError> {
        let ledger = crate::domain::MemoryLedger::new(250);
        let registry = Registry::with_memory(8, Some((ledger.clone(), 100)));
        let first = register(&registry, "live/one")?;
        let _second = register(&registry, "live/two")?;
        assert_eq!(ledger.committed(), 200);

        // A third would take the committed total past 250, although the
        // session count still has room.
        let full = crate::domain::MemoryFull {
            requested: 100,
            committed: 200,
            limit: 250,
        };
        assert_registration_rejected(&registry, &grant("live/three"), full.into());

        // A takeover replaces a session that already holds its share, so it
        // is admitted and briefly overcommits while the incumbent drains.
        let takeover = register(&registry, "live/one")?;
        assert_eq!(ledger.committed(), 300);
        drop((first, takeover));
        assert_eq!(ledger.committed(), 100);
        Ok(())
    }

    #[test]
    fn a_full_registry_turns_new_publishers_away() -> Result<(), RegistryError> {
        let registry = Registry::with_capacity(1);
        let _held = register(&registry, "live/one")?;

        assert_registration_rejected(
            &registry,
            &grant("live/two"),
            AtCapacity { maximum: 1 }.into(),
        );
        Ok(())
    }

    #[test]
    fn capacity_never_blocks_a_publisher_reclaiming_its_own_stream() -> Result<(), RegistryError> {
        let registry = Registry::with_capacity(1);
        let (_incumbent, incumbent_stop) = register_with_stop(&registry, "live/one")?;

        assert_eq!(registry.preflight(&grant("live/one")), Ok(()));
        let takeover = register(&registry, "live/one")?;

        assert_eq!(takeover.displaced(), 1);
        assert_eq!(incumbent_stop.reason(), Some(StopReason::Replaced));
        Ok(())
    }

    #[test]
    fn only_one_takeover_may_wait_for_a_stream_to_drain() -> Result<(), RegistryError> {
        let registry = Registry::with_capacity(1);
        let _incumbent = register(&registry, "live/one")?;
        let _takeover = register(&registry, "live/one")?;

        assert_registration_rejected(
            &registry,
            &grant("live/one"),
            RegistryError::TakeoverInProgress {
                stream: StreamId::new("live/one"),
            },
        );
        Ok(())
    }

    #[test]
    fn a_policy_can_protect_the_incumbent_from_takeover() -> Result<(), RegistryError> {
        let registry = Registry::default();
        let (_incumbent, incumbent_stop) = register_with_stop(&registry, "live/one")?;
        let mut denied = grant("live/one");
        denied.policy.takeovers = TakeoverPolicy::Deny;

        assert_registration_rejected(
            &registry,
            &denied,
            RegistryError::AlreadyPublished {
                stream: StreamId::new("live/one"),
            },
        );
        assert_eq!(incumbent_stop.reason(), None);
        assert_eq!(registry.len(), 1);
        Ok(())
    }

    #[test]
    fn shutdown_stops_every_session() -> Result<(), RegistryError> {
        let registry = Registry::default();
        let (_first, first_stop) = register_with_stop(&registry, "live/one")?;
        let (_second, second_stop) = register_with_stop(&registry, "live/two")?;

        registry.stop_all(StopReason::Cancelled);

        assert_eq!(first_stop.reason(), Some(StopReason::Cancelled));
        assert_eq!(second_stop.reason(), Some(StopReason::Cancelled));
        Ok(())
    }
}
