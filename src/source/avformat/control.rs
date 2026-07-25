use std::{
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    time::Instant,
};

use parking_lot::Mutex;

use crate::source::InputState;

use super::input::AvformatInterrupt;

const OPEN: u8 = 0;
const CLOSED: u8 = 1;
const INTERRUPTED: u8 = 2;

pub struct Control {
    cancelled: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    probe_remaining: Mutex<Option<usize>>,
    probe_exceeded: AtomicBool,
    terminal: AtomicU8,
    input_error: Mutex<Option<String>>,
}

impl Control {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            deadline: Mutex::new(None),
            probe_remaining: Mutex::new(None),
            probe_exceeded: AtomicBool::new(false),
            terminal: AtomicU8::new(OPEN),
            input_error: Mutex::new(None),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn set_deadline(&self, deadline: Option<Instant>) {
        *self.deadline.lock() = deadline;
    }

    pub fn interrupted(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self.probe_exceeded.load(Ordering::Acquire)
            || self
                .deadline
                .lock()
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn begin_probe(&self, maximum_bytes: usize) {
        *self.probe_remaining.lock() = Some(maximum_bytes);
        self.probe_exceeded.store(false, Ordering::Release);
    }

    pub fn finish_probe(&self) {
        *self.probe_remaining.lock() = None;
    }

    pub fn limit_read(&self, requested: usize) -> usize {
        let remaining = self.probe_remaining.lock();
        match *remaining {
            Some(0) => {
                self.probe_exceeded.store(true, Ordering::Release);
                0
            }
            Some(remaining) => requested.min(remaining),
            None => requested,
        }
    }

    pub fn record_read(&self, read: usize) {
        if let Some(remaining) = self.probe_remaining.lock().as_mut() {
            *remaining = remaining.saturating_sub(read);
        }
    }

    pub fn probe_exceeded(&self) -> bool {
        self.probe_exceeded.load(Ordering::Acquire)
    }

    pub fn set_terminal(&self, state: InputState) {
        let encoded = match state {
            InputState::Open => OPEN,
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

    pub fn set_input_error(&self, error: String) {
        *self.input_error.lock() = Some(error);
    }

    pub fn take_input_error(&self) -> Option<String> {
        self.input_error.lock().take()
    }
}

impl AvformatInterrupt for Control {
    fn interrupted(&self) -> bool {
        Self::interrupted(self)
    }
}
