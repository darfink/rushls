use tokio::sync::watch;

/// Why something outside the pipeline asked a session to stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// Operator or shutdown request.
    Cancelled,
    /// A new publisher took the stream identity over.
    Replaced,
}

/// A one-shot, cloneable stop request.
///
/// Held by the session that observes it and by the registry that may fire it,
/// which is what makes takeover expressible: registering a stream signals the
/// incumbent rather than failing the newcomer.
#[derive(Clone, Debug)]
pub struct StopToken {
    state: watch::Sender<Option<StopReason>>,
}

impl StopToken {
    pub fn new() -> Self {
        Self {
            state: watch::Sender::new(None),
        }
    }

    /// Requests a stop. The first reason wins; later requests are ignored.
    pub fn stop(&self, reason: StopReason) {
        self.state.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
    }

    pub fn reason(&self) -> Option<StopReason> {
        *self.state.borrow()
    }

    /// Resolves once a stop has been requested, and never otherwise.
    ///
    /// Safe to poll in a `select!` arm: it observes the current state before
    /// waiting, so a stop that fired before this was called still resolves.
    pub async fn stopped(&self) -> StopReason {
        let mut updates = self.state.subscribe();
        loop {
            let current = *updates.borrow_and_update();
            if let Some(reason) = current {
                return reason;
            }
            if updates.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }
}

impl Default for StopToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_stop_that_already_fired_still_resolves() {
        let token = StopToken::new();
        token.stop(StopReason::Cancelled);

        let reason = tokio::time::timeout(Duration::from_secs(1), token.stopped())
            .await
            .expect("an already-stopped token resolves immediately");
        assert_eq!(reason, StopReason::Cancelled);
    }

    #[tokio::test]
    async fn a_waiter_wakes_on_the_first_reason_only() {
        let token = StopToken::new();
        let waiter = {
            let token = token.clone();
            tokio::spawn(async move { token.stopped().await })
        };
        tokio::task::yield_now().await;

        token.stop(StopReason::Replaced);
        token.stop(StopReason::Cancelled);

        let reason = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the waiter wakes")
            .expect("the waiter task succeeds");
        assert_eq!(reason, StopReason::Replaced);
        assert_eq!(token.reason(), Some(StopReason::Replaced));
    }
}
