use std::sync::Arc;

use crate::{
    domain::{SessionId, StreamId, TrackCounts},
    observe::{EventSink, SessionEvent, SessionMeters},
};

use super::{Phase, SessionShared, StopToken};

/// Everything a step needs in order to report progress, record volume, and
/// notice that it should stop.
///
/// One handle replaces the per-domain event emitters this used to thread
/// through five separate input structs. Stages that only produce items the
/// driving loop can already see never touch it at all; it exists for the
/// session's own bookkeeping and for the two things no caller can observe from
/// outside — the phase and the stop request.
#[derive(Clone, Debug)]
pub struct SessionContext {
    shared: Arc<SessionShared>,
    events: EventSink,
    stop: StopToken,
}

impl SessionContext {
    pub(super) fn new(shared: Arc<SessionShared>, events: EventSink, stop: StopToken) -> Self {
        Self {
            shared,
            events,
            stop,
        }
    }

    pub fn id(&self) -> SessionId {
        self.shared.id()
    }

    pub fn stream(&self) -> &StreamId {
        self.shared.stream()
    }

    pub fn meters(&self) -> &SessionMeters {
        self.shared.meters()
    }

    pub fn events(&self) -> &EventSink {
        &self.events
    }

    pub fn stop(&self) -> &StopToken {
        &self.stop
    }

    pub fn phase(&self) -> Phase {
        self.shared.phase()
    }

    /// Advances to the next lifecycle phase.
    pub fn enter(&self, phase: Phase) {
        self.shared.set_phase(phase);
    }

    pub fn record_tracks(&self, tracks: TrackCounts) {
        self.shared.set_tracks(tracks);
    }

    pub fn emit(&self, event: SessionEvent) {
        self.events.emit(event);
    }
}
