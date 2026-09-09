//! Records what the origin said about itself while a case ran.
//!
//! Some failures are reported and then discarded. A drain that cannot flush
//! emits [`SessionEvent::DrainFailed`] and the session still returns
//! [`SessionOutcome::Ended`], because one publisher whose last frames could not be
//! written is not a reason for a node to declare itself broken. That is the
//! right call for an origin serving thousands of streams and the wrong one for
//! a test, which has exactly one publisher and wants to hear about it.
//!
//! So the suite listens. Without this, a session that failed to finalize its
//! playlists is indistinguishable from one that finalized them correctly until
//! Apple happens to notice, several layers later, that a blocking reload never
//! returns.

use std::sync::Arc;

use parking_lot::Mutex;
use rushls::{
    domain::{SessionId, StreamId},
    observe::{EventObserver, Events, SessionEvent, StreamEvent},
};

#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<String>>>);

impl Recorder {
    pub fn events(&self) -> Events {
        Events::new(Arc::new(self.clone()))
    }

    /// Every event whose name says the origin could not do something.
    ///
    /// Matched on the debug spelling rather than on the enum so a variant added
    /// later is caught by default. An observability enum grows faster than a
    /// test suite reads it, and the failure mode of missing one is silence.
    pub fn failures(&self) -> Vec<String> {
        self.0
            .lock()
            .iter()
            .filter(|event| {
                let lowered = event.to_ascii_lowercase();
                lowered.contains("failed") || lowered.contains("rejected")
            })
            .cloned()
            .collect()
    }
}

impl EventObserver for Recorder {
    fn observe(&self, _session: SessionId, event: SessionEvent) {
        self.0.lock().push(format!("{event:?}"));
    }

    fn observe_stream(&self, _stream: StreamId, event: StreamEvent) {
        self.0.lock().push(format!("{event:?}"));
    }
}
