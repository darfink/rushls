//! Cancellation and probe budget for the MPEG-TS demux task.
//!
//! Cancel is wait-free for the session Drop path. The demux task is aborted
//! when the source is dropped, which cancels a pending SRT receive.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const NOT_PROBING: usize = usize::MAX;

pub struct Control {
    cancelled: AtomicBool,
    probe_remaining: AtomicUsize,
    probe_exceeded: AtomicBool,
}

impl Control {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            probe_remaining: AtomicUsize::new(NOT_PROBING),
            probe_exceeded: AtomicBool::new(false),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
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

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
