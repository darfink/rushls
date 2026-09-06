//! Cancellation, probe budget, and deadline for the MPEG-TS worker.
//!
//! Same shape as the AVFormat control block: the interrupt is wait-free so a
//! cancelled session can unwind a blocking SRT receive without taking a lock.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use crate::source::byte::ByteInterrupt;

const NOT_PROBING: usize = usize::MAX;

pub struct Control {
    cancelled: AtomicBool,
    deadline_nanos: AtomicU64,
    origin: Instant,
    probe_remaining: AtomicUsize,
    probe_exceeded: AtomicBool,
}

impl Control {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            deadline_nanos: AtomicU64::new(0),
            origin: Instant::now(),
            probe_remaining: AtomicUsize::new(NOT_PROBING),
            probe_exceeded: AtomicBool::new(false),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn set_deadline(&self, deadline: Option<Instant>) {
        let nanos = deadline.map_or(0, |deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(self.origin)
                    .as_nanos()
                    .max(1),
            )
            .unwrap_or(u64::MAX)
        });
        self.deadline_nanos.store(nanos, Ordering::Release);
    }

    pub fn interrupted(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self.probe_exceeded.load(Ordering::Acquire)
            || self.deadline_elapsed()
    }

    fn deadline_elapsed(&self) -> bool {
        let nanos = self.deadline_nanos.load(Ordering::Acquire);
        nanos != 0 && u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX) >= nanos
    }

    pub fn begin_probe(&self, maximum_bytes: usize) {
        self.probe_remaining
            .store(maximum_bytes.min(NOT_PROBING - 1), Ordering::Release);
        self.probe_exceeded.store(false, Ordering::Release);
    }

    pub fn finish_probe(&self) {
        self.probe_remaining.store(NOT_PROBING, Ordering::Release);
        self.probe_exceeded.store(false, Ordering::Release);
    }

    pub fn limit_read(&self, requested: usize) -> usize {
        let remaining = self.probe_remaining.load(Ordering::Acquire);
        if remaining == NOT_PROBING {
            return requested;
        }
        if remaining == 0 {
            self.probe_exceeded.store(true, Ordering::Release);
            return 0;
        }
        requested.min(remaining)
    }

    pub fn record_read(&self, read: usize) {
        let mut remaining = self.probe_remaining.load(Ordering::Acquire);
        loop {
            if remaining == NOT_PROBING {
                return;
            }
            let next = remaining.saturating_sub(read);
            match self.probe_remaining.compare_exchange_weak(
                remaining,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => remaining = current,
            }
        }
    }

    pub fn probe_exceeded(&self) -> bool {
        self.probe_exceeded.load(Ordering::Acquire)
    }

    pub fn deadline_exceeded(&self) -> bool {
        self.deadline_elapsed()
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl ByteInterrupt for Control {
    fn interrupted(&self) -> bool {
        Self::interrupted(self)
    }
}
