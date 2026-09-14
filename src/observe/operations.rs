//! Bounded operation timing with cancellation-safe in-flight accounting.

use super::DurationHistogram;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    BlockingReload,
    InitialReadiness,
    HintedPart,
    DiskRead,
    PlaylistProjection,
    StoreBackpressure,
    DiskWrite,
}
impl Operation {
    pub const ALL: [Self; 7] = [
        Self::BlockingReload,
        Self::InitialReadiness,
        Self::HintedPart,
        Self::DiskRead,
        Self::PlaylistProjection,
        Self::StoreBackpressure,
        Self::DiskWrite,
    ];
    pub const fn name(self) -> &'static str {
        match self {
            Self::BlockingReload => "blocking_reload",
            Self::InitialReadiness => "initial_readiness",
            Self::HintedPart => "hinted_part",
            Self::DiskRead => "disk_read",
            Self::PlaylistProjection => "playlist_projection",
            Self::StoreBackpressure => "store_backpressure",
            Self::DiskWrite => "disk_write",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum OperationOutcome {
    Completed,
    Ended,
    Expired,
    Error,
    Cancelled,
}
impl OperationOutcome {
    pub const ALL: [Self; 5] = [
        Self::Completed,
        Self::Ended,
        Self::Expired,
        Self::Error,
        Self::Cancelled,
    ];
    pub const fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Ended => "ended",
            Self::Expired => "expired",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct OperationSnapshot {
    pub started: u64,
    pub in_flight: usize,
    pub outcomes: [u64; 5],
    pub duration: DurationHistogram,
}
#[derive(Clone, Debug, Default)]
pub struct OperationMeters(Arc<Mutex<[OperationSnapshot; 7]>>);
impl OperationMeters {
    pub fn start(&self, operation: Operation) -> OperationGuard {
        let mut counters = self.0.lock();
        counters[operation as usize].started += 1;
        counters[operation as usize].in_flight += 1;
        OperationGuard {
            meters: self.clone(),
            operation,
            started: Instant::now(),
            finished: false,
        }
    }
    pub fn snapshot(&self) -> [OperationSnapshot; 7] {
        self.0.lock().clone()
    }
}
pub struct OperationGuard {
    meters: OperationMeters,
    operation: Operation,
    started: Instant,
    finished: bool,
}
impl OperationGuard {
    pub fn finish(mut self, outcome: OperationOutcome) {
        self.record(outcome);
    }
    fn record(&mut self, outcome: OperationOutcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut counters = self.meters.0.lock();
        let operation = &mut counters[self.operation as usize];
        operation.in_flight -= 1;
        operation.outcomes[outcome as usize] += 1;
        operation.duration.observe(self.started.elapsed());
    }
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.record(OperationOutcome::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test(start_paused = true)]
    async fn cancellation_and_timeout_share_the_same_started_population() {
        let meters = OperationMeters::default();
        let held = meters.start(Operation::InitialReadiness);
        assert_eq!(meters.snapshot()[1].in_flight, 1);
        tokio::time::advance(Duration::from_secs(3)).await;
        drop(held);
        let expired = meters.start(Operation::InitialReadiness);
        tokio::time::advance(Duration::from_secs(2)).await;
        expired.finish(OperationOutcome::Expired);
        let snapshot = &meters.snapshot()[1];
        assert_eq!(snapshot.started, 2);
        assert_eq!(snapshot.in_flight, 0);
        assert_eq!(snapshot.outcomes[OperationOutcome::Cancelled as usize], 1);
        assert_eq!(snapshot.outcomes[OperationOutcome::Expired as usize], 1);
        assert_eq!(snapshot.duration.count, 2);
        assert!((snapshot.duration.sum - 5.0).abs() < f64::EPSILON);
    }
}
