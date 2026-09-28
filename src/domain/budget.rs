//! Shared publisher allocation accounting. Reservations never wait: retained
//! media may need more input before it can release memory.
use std::sync::{
    Arc, Weak,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Debug)]
pub struct PipelineBudget(Arc<State>);

#[derive(Debug)]
struct State {
    limit: usize,
    working_allowance: usize,
    working: AtomicUsize,
    origins: [AtomicUsize; 5],
    used: AtomicUsize,
    peak: AtomicUsize,
    failures: AtomicUsize,
    allocations: parking_lot::Mutex<std::collections::BTreeMap<usize, (usize, Weak<Reservation>)>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "pipeline memory exhausted in {stage}: requested {requested} bytes, using {used} of {limit} (reservation ceiling {ceiling})"
)]
pub struct BudgetExceeded {
    pub stage: &'static str,
    pub requested: usize,
    pub used: usize,
    pub limit: usize,
    pub ceiling: usize,
}

/// An allocation's charge. Move this with the allocation; share it when the
/// allocation is shared. Dropping a queue entry must not release live bytes.
#[derive(Debug)]
pub struct Reservation {
    budget: PipelineBudget,
    bytes: usize,
    address: Option<usize>,
    // Pin the backing bytes until the address index has been removed. Without
    // this, an allocator could reuse an address before the last lease drops.
    backing: Option<bytes::Bytes>,
    working: bool,
    origin: usize,
}

impl PipelineBudget {
    pub const ORIGINS: [&'static str; 5] = ["transport", "demux", "mux", "subtitle", "other"];
    pub const MIN_LIMIT: usize = 64 * 1024 * 1024;
    pub const DEFAULT_LIMIT: usize = 128 * 1024 * 1024;

    pub fn new(limit: usize) -> Self {
        Self::with_working_allowance(limit, 0)
    }

    pub fn with_working_allowance(limit: usize, working_allowance: usize) -> Self {
        assert!(working_allowance <= limit);
        Self(Arc::new(State {
            limit,
            working_allowance,
            working: AtomicUsize::new(0),
            origins: std::array::from_fn(|_| AtomicUsize::new(0)),
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            allocations: parking_lot::Mutex::new(std::collections::BTreeMap::new()),
        }))
    }

    /// Recover an existing backing-allocation lease when an adapter emits a
    /// slice. Weak entries never extend the lifetime of media buffers.
    pub fn charge_bytes(
        &self,
        data: &bytes::Bytes,
        overhead: usize,
        stage: &'static str,
    ) -> Result<Arc<Reservation>, BudgetExceeded> {
        let address = data.as_ptr() as usize;
        let mut allocations = self.0.allocations.lock();
        if let Some((&start, (length, charge))) = allocations.range(..=address).next_back()
            && address.saturating_sub(start).saturating_add(data.len()) <= *length
            && let Some(charge) = charge.upgrade()
        {
            return Ok(charge);
        }
        let mut reservation = self.try_reserve(data.len().saturating_add(overhead), stage)?;
        // Empty buffers share sentinel addresses, so they must not be indexed.
        if !data.is_empty() {
            reservation.address = Some(address);
            reservation.backing = Some(data.clone());
        }
        let charge = Arc::new(reservation);
        if !data.is_empty() {
            allocations.insert(address, (data.len(), Arc::downgrade(&charge)));
        }
        Ok(charge)
    }

    pub fn origins(&self) -> [u64; 5] {
        std::array::from_fn(|index| self.0.origins[index].load(Ordering::Relaxed) as u64)
    }

    pub fn working(&self) -> usize {
        self.0.working.load(Ordering::Relaxed)
    }
    pub fn retained_limit(&self) -> usize {
        self.0.limit - self.0.working_allowance
    }

    pub fn register(&self, data: &bytes::Bytes, mut reservation: Reservation) -> Arc<Reservation> {
        if data.is_empty() {
            return Arc::new(reservation);
        }
        let address = data.as_ptr() as usize;
        reservation.address = Some(address);
        reservation.backing = Some(data.clone());
        let charge = Arc::new(reservation);
        self.0
            .allocations
            .lock()
            .insert(address, (data.len(), Arc::downgrade(&charge)));
        charge
    }

    pub fn limit(&self) -> usize {
        self.0.limit
    }
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::Relaxed)
    }
    pub fn peak(&self) -> usize {
        self.0.peak.load(Ordering::Relaxed)
    }
    pub fn failures(&self) -> usize {
        self.0.failures.load(Ordering::Relaxed)
    }

    /// Reserve before allocation. Relaxed ordering suffices: the counter does
    /// not publish data, and ownership synchronizes the allocations themselves.
    pub fn try_reserve(
        &self,
        bytes: usize,
        stage: &'static str,
    ) -> Result<Reservation, BudgetExceeded> {
        self.reserve(bytes, stage, false)
    }

    /// Temporary copies may use protected headroom, but may never remain as
    /// retained media without returning to the ordinary allowance.
    pub fn try_reserve_working(
        &self,
        bytes: usize,
        stage: &'static str,
    ) -> Result<Reservation, BudgetExceeded> {
        self.reserve(bytes, stage, true)
    }

    fn reserve(
        &self,
        bytes: usize,
        stage: &'static str,
        working: bool,
    ) -> Result<Reservation, BudgetExceeded> {
        let origin = match stage {
            "RTMP receive" | "RTMP coalescing" | "rtmp ingress" => 0,
            "demux" => 1,
            "mux output" | "CMAF serialization" => 2,
            "subtitle cues" | "subtitle output" => 3,
            _ => 4,
        };
        let ceiling = if working {
            self.limit()
        } else {
            self.retained_limit()
        };
        let mut used = self.used();
        loop {
            let Some(next) = used.checked_add(bytes).filter(|next| *next <= ceiling) else {
                self.0.failures.fetch_add(1, Ordering::Relaxed);
                return Err(BudgetExceeded {
                    stage,
                    requested: bytes,
                    used,
                    limit: self.limit(),
                    ceiling,
                });
            };
            match self.0.used.compare_exchange_weak(
                used,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.0.peak.fetch_max(next, Ordering::Relaxed);
                    if working {
                        self.0.working.fetch_add(bytes, Ordering::Relaxed);
                    }
                    self.0.origins[origin].fetch_add(bytes, Ordering::Relaxed);
                    return Ok(Reservation {
                        budget: self.clone(),
                        bytes,
                        address: None,
                        backing: None,
                        working,
                        origin,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

impl Reservation {
    pub fn budget(&self) -> &PipelineBudget {
        &self.budget
    }

    /// Split a charge without a release/reacquire window.
    #[must_use]
    pub fn split(&mut self, bytes: usize) -> Self {
        assert!(
            self.address.is_none(),
            "registered allocations cannot be split"
        );
        assert!(bytes <= self.bytes);
        self.bytes -= bytes;
        Self {
            budget: self.budget.clone(),
            bytes,
            address: None,
            backing: None,
            working: self.working,
            origin: self.origin,
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(address) = self.address {
            let mut allocations = self.budget.0.allocations.lock();
            if allocations
                .get(&address)
                .is_some_and(|(_, charge)| charge.as_ptr() == std::ptr::from_ref(self))
            {
                allocations.remove(&address);
            }
        }
        // Deregister before freeing the backing allocation, and release the
        // quota after freeing it. Both ordering constraints matter under reuse.
        drop(self.backing.take());
        self.budget.0.origins[self.origin].fetch_sub(self.bytes, Ordering::Relaxed);
        if self.working {
            self.budget
                .0
                .working
                .fetch_sub(self.bytes, Ordering::Relaxed);
        }
        self.budget.0.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_and_exhaustion() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(100);
        let mut first = budget.try_reserve(80, "preroll")?;
        let second = first.split(30);
        assert!(budget.try_reserve(21, "mux").is_err());
        assert!(budget.try_reserve(usize::MAX, "mux").is_err());
        drop(first);
        assert_eq!(budget.used(), 30);
        let third = budget.try_reserve(70, "mux")?;
        assert_eq!(budget.peak(), 100);
        drop((second, third));
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn stages_borrow_space_but_cannot_consume_working_headroom() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::with_working_allowance(128, 16);
        let preroll = budget.try_reserve(96, "demux")?;
        let output = budget.try_reserve(16, "mux output")?;
        assert!(budget.try_reserve(1, "demux").is_err());
        let working = budget.try_reserve_working(16, "CMAF serialization")?;
        assert_eq!(budget.used(), 128);
        assert_eq!(budget.working(), 16);
        assert_eq!(budget.origins(), [0, 96, 32, 0, 0]);
        assert!(budget.try_reserve_working(1, "CMAF serialization").is_err());
        drop((working, preroll));
        let borrowed = budget.try_reserve(96, "mux output")?;
        assert_eq!(budget.used(), 112);
        drop((output, borrowed));
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.origins(), [0; 5]);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_releases_reservations() -> Result<(), Box<dyn std::error::Error>> {
        let budget = PipelineBudget::new(100);
        let task_budget = budget.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _charge = task_budget.try_reserve(100, "demux")?;
            let _ = ready.send(());
            std::future::pending::<()>().await;
            Ok::<_, BudgetExceeded>(())
        });
        started.await?;
        assert_eq!(budget.used(), 100);
        let other_publisher = PipelineBudget::new(100);
        let independent = other_publisher.try_reserve(100, "demux")?;
        task.abort();
        assert!(task.await.expect_err("task was cancelled").is_cancelled());
        assert_eq!(budget.used(), 0);
        assert_eq!(other_publisher.used(), 100);
        drop(independent);
        Ok(())
    }

    #[test]
    fn concurrent_reservations_never_overshoot() {
        let budget = PipelineBudget::new(100);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let budget = &budget;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        if let Ok(permit) = budget.try_reserve(30, "test") {
                            assert!(budget.used() <= 100);
                            std::thread::yield_now();
                            drop(permit);
                        }
                    }
                });
            }
        });
        assert_eq!(budget.used(), 0);
        assert!(budget.peak() <= 100);
    }
}
