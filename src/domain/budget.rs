//! Shared publisher allocation accounting. Reservations never wait: retained
//! media may need more input before it can release memory.
//!
//! A completion reserve sits above the ordinary ceiling. Only stages that turn
//! held input into output may use it (see [`Stage::may_use_reserve`]), so
//! ingress and preroll exhaust the ordinary allowance first while in-flight
//! parts can still be finished and handed to storage.
use std::sync::{
    Arc, Weak,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Debug)]
pub struct PipelineBudget(Arc<State>);

/// Node-wide committed memory: the sum of the budgets granted to active
/// publishers and stored streams, against `memory.total`.
///
/// Budgets are ceilings, not allocations, so this commits them rather than
/// measuring use. That is what makes `total` a guarantee: however the budgets
/// fill, they can never add up to more. Commits never wait; a refused commit
/// refuses the publication.
#[derive(Clone, Debug)]
pub struct MemoryLedger(Arc<LedgerState>);

#[derive(Debug)]
struct LedgerState {
    limit: usize,
    committed: AtomicUsize,
}

/// Memory committed on a [`MemoryLedger`], returned when dropped.
#[derive(Debug)]
pub struct Commitment {
    ledger: MemoryLedger,
    bytes: usize,
}

/// A commitment that would take the ledger past its limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "node memory is fully committed: {requested} more bytes would exceed {committed} of {limit}"
)]
pub struct MemoryFull {
    pub requested: usize,
    pub committed: usize,
    pub limit: usize,
}

impl MemoryLedger {
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(LedgerState {
            limit,
            committed: AtomicUsize::new(0),
        }))
    }

    pub fn limit(&self) -> usize {
        self.0.limit
    }

    pub fn committed(&self) -> usize {
        self.0.committed.load(Ordering::Relaxed)
    }

    pub fn commit(&self, bytes: usize) -> Result<Commitment, MemoryFull> {
        let mut committed = self.committed();
        loop {
            let next = committed
                .checked_add(bytes)
                .filter(|next| *next <= self.0.limit)
                .ok_or(MemoryFull {
                    requested: bytes,
                    committed,
                    limit: self.0.limit,
                })?;
            match self.0.committed.compare_exchange_weak(
                committed,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(Commitment {
                        ledger: self.clone(),
                        bytes,
                    });
                }
                Err(current) => committed = current,
            }
        }
    }

    /// Commits past the limit. Only for a takeover: the incumbent it replaces
    /// is already stopping and returns its own commitment within its drain.
    pub fn commit_over(&self, bytes: usize) -> Commitment {
        self.0.committed.fetch_add(bytes, Ordering::Relaxed);
        Commitment {
            ledger: self.clone(),
            bytes,
        }
    }
}

// Identity, not value: two ledgers are the same only if they share state.
impl PartialEq for MemoryLedger {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for MemoryLedger {}

impl Drop for Commitment {
    fn drop(&mut self) {
        self.ledger
            .0
            .committed
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Where a reservation is requested. Error messages print the stage; metrics
/// aggregate it into a coarser [`Origin`] so label cardinality stays fixed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
pub enum Stage {
    #[display("RTMP receive")]
    RtmpReceive,
    #[display("RTMP coalescing")]
    RtmpCoalescing,
    #[display("RTMP ingress")]
    RtmpIngress,
    #[display("MPEG-TS read")]
    MpegTsRead,
    /// Frames `moq_net` holds in its receive cache, measured rather than
    /// reserved before allocation: the dependency allocates them itself.
    #[display("MoQ receive cache")]
    MoqCache,
    #[display("demux")]
    Demux,
    #[display("normalization")]
    Normalization,
    #[display("mux output")]
    MuxOutput,
    #[display("subtitle cues")]
    SubtitleCues,
    #[display("subtitle output")]
    SubtitleOutput,
}

impl Stage {
    pub fn origin(self) -> Origin {
        match self {
            Self::RtmpReceive
            | Self::RtmpCoalescing
            | Self::RtmpIngress
            | Self::MpegTsRead
            | Self::MoqCache => Origin::Transport,
            Self::Demux => Origin::Demux,
            Self::Normalization => Origin::Normalization,
            Self::MuxOutput => Origin::Mux,
            Self::SubtitleCues | Self::SubtitleOutput => Origin::Subtitle,
        }
    }

    /// Output reservations free their input once published. Letting them use
    /// the completion reserve means a full pipeline can still drain.
    pub fn may_use_reserve(self) -> bool {
        matches!(self, Self::MuxOutput | Self::SubtitleOutput)
    }
}

/// Metric attribution for accounted bytes. Discriminants index the counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
pub enum Origin {
    #[display("transport")]
    Transport,
    #[display("demux")]
    Demux,
    #[display("normalization")]
    Normalization,
    #[display("mux")]
    Mux,
    #[display("subtitle")]
    Subtitle,
}

impl Origin {
    pub const ALL: [Self; 5] = [
        Self::Transport,
        Self::Demux,
        Self::Normalization,
        Self::Mux,
        Self::Subtitle,
    ];
}

#[derive(Debug)]
struct State {
    limit: usize,
    reserve: usize,
    origins: [AtomicUsize; Origin::ALL.len()],
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
    pub stage: Stage,
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
    origin: Origin,
}

impl PipelineBudget {
    pub const MIN_LIMIT: usize = 64 * 1024 * 1024;
    pub const DEFAULT_LIMIT: usize = 128 * 1024 * 1024;
    /// Largest completion reserve carved out of a publisher budget.
    pub const MAX_RESERVE: usize = 16 * 1024 * 1024;

    pub fn new(limit: usize) -> Self {
        Self::with_reserve(limit, 0)
    }

    /// A publisher budget: one quarter of the limit, up to [`Self::MAX_RESERVE`],
    /// is kept for finishing in-flight output. `None` means unlimited.
    pub fn for_publisher(limit: Option<usize>) -> Self {
        match limit {
            Some(limit) => Self::with_reserve(limit, (limit / 4).min(Self::MAX_RESERVE)),
            None => Self::unlimited(),
        }
    }

    /// No ceiling, but still accounted, so usage and peak metrics stay useful
    /// in trusted deployments that opt out of enforcement.
    pub fn unlimited() -> Self {
        Self::with_reserve(usize::MAX, 0)
    }

    pub fn is_unlimited(&self) -> bool {
        self.0.limit == usize::MAX
    }

    pub fn with_reserve(limit: usize, reserve: usize) -> Self {
        assert!(reserve <= limit);
        Self(Arc::new(State {
            limit,
            reserve,
            origins: std::array::from_fn(|_| AtomicUsize::new(0)),
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            allocations: parking_lot::Mutex::new(std::collections::BTreeMap::new()),
        }))
    }

    /// Recover an existing backing-allocation lease when an adapter emits a
    /// slice. Weak entries never extend the lifetime of media buffers.
    /// Only the allocation is charged here; per-owner overhead must be
    /// reserved separately so that it is also counted when a lease is reused.
    pub fn charge_bytes(
        &self,
        data: &bytes::Bytes,
        stage: Stage,
    ) -> Result<Arc<Reservation>, BudgetExceeded> {
        let address = data.as_ptr() as usize;
        let mut allocations = self.0.allocations.lock();
        if let Some((&start, (length, charge))) = allocations.range(..=address).next_back()
            && address.saturating_sub(start).saturating_add(data.len()) <= *length
            && let Some(charge) = charge.upgrade()
        {
            return Ok(charge);
        }
        let mut reservation = self.try_reserve(data.len(), stage)?;
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

    pub fn origins(&self) -> [u64; Origin::ALL.len()] {
        std::array::from_fn(|index| self.0.origins[index].load(Ordering::Relaxed) as u64)
    }

    /// Ceiling for stages that may not use the completion reserve.
    pub fn ordinary_limit(&self) -> usize {
        self.0.limit - self.0.reserve
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
    pub fn try_reserve(&self, bytes: usize, stage: Stage) -> Result<Reservation, BudgetExceeded> {
        let origin = stage.origin();
        let ceiling = if stage.may_use_reserve() {
            self.limit()
        } else {
            self.ordinary_limit()
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
                    self.0.origins[origin as usize].fetch_add(bytes, Ordering::Relaxed);
                    return Ok(Reservation {
                        budget: self.clone(),
                        bytes,
                        address: None,
                        backing: None,
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

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Merge another unregistered charge into this one, so a charge can grow
    /// sample by sample and later be split to an exact output size.
    pub fn absorb(&mut self, mut other: Self) {
        assert!(
            Arc::ptr_eq(&self.budget.0, &other.budget.0),
            "charges from different budgets cannot merge"
        );
        assert_eq!(self.origin, other.origin, "charges must share an origin");
        assert!(
            self.address.is_none() && other.address.is_none(),
            "registered allocations cannot merge"
        );
        self.bytes += std::mem::take(&mut other.bytes);
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
        self.budget.0.origins[self.origin as usize].fetch_sub(self.bytes, Ordering::Relaxed);
        self.budget.0.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_and_exhaustion() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::new(100);
        let mut first = budget.try_reserve(80, Stage::Demux)?;
        let second = first.split(30);
        assert!(budget.try_reserve(21, Stage::MuxOutput).is_err());
        assert!(budget.try_reserve(usize::MAX, Stage::MuxOutput).is_err());
        drop(first);
        assert_eq!(budget.used(), 30);
        let third = budget.try_reserve(70, Stage::MuxOutput)?;
        assert_eq!(budget.peak(), 100);
        drop((second, third));
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[test]
    fn only_output_stages_may_use_the_completion_reserve() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::with_reserve(128, 16);
        let preroll = budget.try_reserve(96, Stage::Demux)?;
        let mut output = budget.try_reserve(16, Stage::MuxOutput)?;
        // Input stages stop at the ordinary ceiling ...
        assert!(budget.try_reserve(1, Stage::Demux).is_err());
        // ... while output can still finish inside the reserve.
        output.absorb(budget.try_reserve(16, Stage::MuxOutput)?);
        assert_eq!(output.bytes(), 32);
        assert_eq!(budget.used(), 128);
        assert_eq!(budget.origins(), [0, 96, 0, 32, 0]);
        assert!(budget.try_reserve(1, Stage::MuxOutput).is_err());
        let retained = output.split(20);
        drop((output, preroll));
        assert_eq!(budget.used(), 20);
        drop(retained);
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.origins(), [0; 5]);
        Ok(())
    }

    #[test]
    fn the_ledger_refuses_commitments_past_its_limit() -> Result<(), MemoryFull> {
        let ledger = MemoryLedger::new(100);
        let first = ledger.commit(60)?;
        assert_eq!(
            ledger.commit(41).map(|_| ()),
            Err(MemoryFull {
                requested: 41,
                committed: 60,
                limit: 100
            })
        );
        let second = ledger.commit(40)?;
        assert_eq!(ledger.committed(), 100);
        // A takeover may overcommit briefly while its incumbent drains.
        let takeover = ledger.commit_over(60);
        drop((first, second));
        assert_eq!(ledger.committed(), 60);
        drop(takeover);
        assert_eq!(ledger.committed(), 0);
        Ok(())
    }

    #[test]
    fn unlimited_budgets_still_account() -> Result<(), BudgetExceeded> {
        let budget = PipelineBudget::for_publisher(None);
        assert!(budget.is_unlimited());
        let charge = budget.try_reserve(usize::MAX / 2, Stage::Demux)?;
        assert_eq!(budget.peak(), usize::MAX / 2);
        drop(charge);
        assert_eq!(budget.used(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_releases_reservations() -> Result<(), Box<dyn std::error::Error>> {
        let budget = PipelineBudget::new(100);
        let task_budget = budget.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _charge = task_budget.try_reserve(100, Stage::Demux)?;
            let _ = ready.send(());
            std::future::pending::<()>().await;
            Ok::<_, BudgetExceeded>(())
        });
        started.await?;
        assert_eq!(budget.used(), 100);
        let other_publisher = PipelineBudget::new(100);
        let independent = other_publisher.try_reserve(100, Stage::Demux)?;
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
                        if let Ok(permit) = budget.try_reserve(30, Stage::Demux) {
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
