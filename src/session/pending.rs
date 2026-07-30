//! How many publishers may be part-way through admission at once.
//!
//! [`Registry`](super::Registry) bounds sessions that have registered.
//! Nothing bounded the window before that: between accepting a socket and
//! registering a session, a connection costs a task, a socket, and — once
//! admission consults a remote service — an outbound request, while nothing
//! has yet proved it is a publisher at all. `maximum_sessions` cannot cover
//! that window, because it counts only what got through it.
//!
//! A permit is released when admission concludes rather than when the session
//! ends. Holding it for a session's whole life would make this a second, lower
//! session cap, and would let ordinary long-lived publishers consume the
//! headroom that exists to absorb unauthenticated connections.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Capacity for connections accepted but not yet admitted.
///
/// Cloning shares one budget, so a listener can hand it to every connection it
/// accepts. Each ingest listener owns its own, which keeps a flood on one
/// transport from denying admission on the other.
#[derive(Clone, derive_more::Debug)]
#[debug("PendingPublishers {{ available: {} }}", semaphore.available_permits())]
pub struct PendingPublishers {
    #[debug(skip)]
    semaphore: Arc<Semaphore>,
}

impl PendingPublishers {
    pub fn new(maximum: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(maximum)),
        }
    }

    /// Waits until admission capacity exists, then reserves one slot.
    ///
    /// Cancel-safe: a caller that abandons this in a `select!` reserves
    /// nothing, so a listener may race it against its shutdown signal.
    pub async fn reserve(&self) -> PendingPermit {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("the admission semaphore is never closed");
        PendingPermit(Some(permit))
    }

    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// One reserved admission slot, released when dropped.
#[derive(derive_more::Debug)]
#[debug("PendingPermit")]
pub struct PendingPermit(
    #[debug(skip)]
    #[expect(
        dead_code,
        reason = "held only so that dropping it returns the reservation"
    )]
    Option<OwnedSemaphorePermit>,
);

impl PendingPermit {
    /// A permit that reserves nothing.
    ///
    /// For callers that bound admission themselves, or that are not exposed to
    /// the network at all and have nothing to bound.
    pub fn unlimited() -> Self {
        Self(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reservations_are_bounded_and_returned_when_dropped() {
        let pending = PendingPublishers::new(2);

        let first = pending.reserve().await;
        let second = pending.reserve().await;
        assert_eq!(pending.available(), 0);

        // A third caller waits rather than proceeding, which is what keeps a
        // surplus of connections in the listener backlog.
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, pending.reserve())
                .await
                .is_err()
        );

        drop(first);
        assert_eq!(pending.available(), 1);
        let _third = pending.reserve().await;
        assert_eq!(pending.available(), 0);

        drop(second);
        assert_eq!(pending.available(), 1);
    }
}
