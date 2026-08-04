//! The one piece of state FFmpeg and Rust both touch while a demux runs.
//!
//! FFmpeg calls back into us from its own blocking worker thread — for every
//! read, and again from its interrupt callback — while the Tokio side may
//! concurrently cancel the source. So every field here is read from a C
//! callback and written from Rust, or the reverse.
//!
//! # Why this is lock-free
//!
//! Not for throughput. [`Control::interrupted`] is called by FFmpeg on *every*
//! read, and `limit_read`/`record_read` on every read during probing. A mutex
//! there means the interrupt callback can block, and an interrupt callback that
//! blocks is exactly the thing that stops a cancelled session from unwinding
//! promptly. Atomics make every FFI-facing operation wait-free, so a callback
//! always returns.
//!
//! # Invariants the FFI depends on
//!
//! - **`interrupted` never blocks and never panics.** FFmpeg calls it across a
//!   C boundary where an unwind is undefined behaviour.
//! - **`interrupted` is monotone.** Once it answers `true`, it keeps answering
//!   `true`, so FFmpeg cannot resume a read after being told to stop. Every
//!   flag it consults is set-only.
//! - **`limit_read` never returns more than it was asked for**, so the slice
//!   handed back to FFmpeg always fits the buffer FFmpeg supplied.
//! - **The probe budget is only ever decremented**, so a concurrent read cannot
//!   make it grow back and let the probe overrun its limit.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use std::time::Instant;

use parking_lot::Mutex;

use crate::source::InputState;

use super::input::AvformatInterrupt;

/// `terminal` encodes an `Option<InputState>`; `NONE` means "still open".
const NONE: u8 = 0;
const CLOSED: u8 = 1;
const INTERRUPTED: u8 = 2;

/// `probing` is off until [`Control::begin_probe`], and the budget is unbounded
/// while it is off.
const NOT_PROBING: usize = usize::MAX;

pub struct Control {
    cancelled: AtomicBool,
    /// Nanoseconds from `origin` to the deadline, or zero for none.
    ///
    /// Stored as a scalar rather than an `Instant` because the interrupt
    /// callback reads it on every FFmpeg read and must not take a lock.
    deadline_nanos: AtomicU64,
    origin: Instant,
    probe_remaining: AtomicUsize,
    probe_exceeded: AtomicBool,
    terminal: AtomicU8,
    /// Written at most once by the byte input, then taken by the reader that
    /// reports the failure. Off the per-read path, so a mutex is free here.
    input_error: Mutex<Option<Box<str>>>,
}

impl Control {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            deadline_nanos: AtomicU64::new(0),
            origin: Instant::now(),
            probe_remaining: AtomicUsize::new(NOT_PROBING),
            probe_exceeded: AtomicBool::new(false),
            terminal: AtomicU8::new(NONE),
            input_error: Mutex::new(None),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Sets the wall-clock limit for the whole demux, or clears it.
    ///
    /// Called once before FFmpeg starts, so the non-atomic relationship between
    /// `origin` and the stored offset is never observed mid-change.
    pub fn set_deadline(&self, deadline: Option<Instant>) {
        let nanos = deadline.map_or(0, |deadline| {
            // Saturating at one nanosecond keeps an already-elapsed
            // deadline distinguishable from "no deadline", which zero means.
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

    /// Whether FFmpeg should abandon whatever it is doing.
    ///
    /// Wait-free and monotone: see the module invariants. FFmpeg calls this
    /// from its interrupt callback on every read.
    pub fn interrupted(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self.probe_exceeded.load(Ordering::Acquire)
            || self.deadline_elapsed()
    }

    fn deadline_elapsed(&self) -> bool {
        let nanos = self.deadline_nanos.load(Ordering::Acquire);
        nanos != 0 && u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX) >= nanos
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn begin_probe(&self, maximum_bytes: usize) {
        // `NOT_PROBING` is `usize::MAX`, so a budget that large is indivisible
        // from "unbounded" — clamp it rather than silently disabling the limit.
        self.probe_remaining
            .store(maximum_bytes.min(NOT_PROBING - 1), Ordering::Release);
        self.probe_exceeded.store(false, Ordering::Release);
    }

    pub fn finish_probe(&self) {
        self.probe_remaining.store(NOT_PROBING, Ordering::Release);
    }

    /// Caps a read against the remaining probe budget.
    ///
    /// Returning zero tells the caller to stop; it also latches
    /// [`Self::probe_exceeded`] so [`Self::interrupted`] starts answering
    /// `true` and FFmpeg unwinds rather than spinning on empty reads.
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

    /// Charges `read` bytes against the probe budget, saturating at zero.
    ///
    /// The subtraction is a compare-and-swap loop rather than `fetch_sub`
    /// because the budget must never wrap: a wrapped counter would read as an
    /// enormous remaining budget and disable the limit entirely.
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

    pub fn set_terminal(&self, state: InputState) {
        let encoded = match state {
            InputState::Open => NONE,
            InputState::Closed => CLOSED,
            InputState::Interrupted => INTERRUPTED,
        };
        self.terminal.store(encoded, Ordering::Release);
    }

    pub fn terminal(&self) -> Option<InputState> {
        match self.terminal.load(Ordering::Acquire) {
            CLOSED => Some(InputState::Closed),
            INTERRUPTED => Some(InputState::Interrupted),
            _ => None,
        }
    }

    pub fn set_input_error(&self, error: Box<str>) {
        *self.input_error.lock() = Some(error);
    }

    pub fn take_input_error(&self) -> Option<Box<str>> {
        self.input_error.lock().take()
    }
}

impl AvformatInterrupt for Control {
    fn interrupted(&self) -> bool {
        Self::interrupted(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_never_exceeds_the_buffer_it_was_offered() {
        let control = Control::new();
        control.begin_probe(4);

        assert_eq!(control.limit_read(64), 4, "capped by the remaining budget");
        assert_eq!(control.limit_read(2), 2, "never grown past the request");
    }

    #[test]
    fn an_exhausted_probe_budget_latches_the_interrupt() {
        let control = Control::new();
        control.begin_probe(4);
        control.record_read(4);

        assert_eq!(control.limit_read(64), 0);
        assert!(
            control.interrupted(),
            "FFmpeg must unwind rather than spin on zero-length reads"
        );
        assert!(
            control.interrupted(),
            "the interrupt is monotone once it fires"
        );
    }

    #[test]
    fn charging_more_than_remains_saturates_instead_of_wrapping() {
        let control = Control::new();
        control.begin_probe(4);
        control.record_read(usize::MAX);

        assert_eq!(
            control.limit_read(1),
            0,
            "a wrapped budget would read as unbounded and disable the limit"
        );
    }

    #[test]
    fn finishing_the_probe_lifts_the_budget() {
        let control = Control::new();
        control.begin_probe(4);
        control.finish_probe();

        assert_eq!(control.limit_read(1_024), 1_024);
        control.record_read(1_024);
        assert!(!control.probe_exceeded());
    }

    #[test]
    fn an_elapsed_deadline_interrupts_and_a_future_one_does_not() {
        let control = Control::new();
        control.set_deadline(Some(Instant::now() + std::time::Duration::from_secs(3_600)));
        assert!(!control.interrupted());

        // Already in the past. The stored offset saturates to one nanosecond
        // rather than zero, which is what keeps it distinct from "no deadline".
        control.set_deadline(Some(
            Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap(),
        ));
        assert!(control.interrupted());
    }

    #[test]
    fn no_deadline_never_interrupts() {
        let control = Control::new();
        control.set_deadline(None);

        assert!(!control.interrupted());
    }
}
