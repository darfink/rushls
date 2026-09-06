//! Bounded async queue from the RTMP session onto the packet source.
//!
//! The session is already on Tokio, so this is a byte-budgeted channel rather
//! than a blocking worker. The budget is one large keyframe: the publisher
//! must wait if the pipeline is not draining.

use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use cc_rtmp::{ParsedAudio, ParsedVideo, ValidatedMedia, ValidatedMetadata};
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::source::InputState;

#[derive(Debug)]
pub enum IngressEvent {
    Audio {
        timestamp: u32,
        media: ValidatedMedia<ParsedAudio>,
    },
    Video {
        timestamp: u32,
        media: ValidatedMedia<ParsedVideo>,
    },
    Metadata(ValidatedMetadata),
    /// AMF0 script-data (`onCaption` / `onTextData` and anything else).
    Script {
        timestamp: u32,
        payload: Bytes,
    },
    End(InputState),
    Failed(Box<str>),
}

impl IngressEvent {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Audio { media, .. } => media.raw.len(),
            Self::Video { media, .. } => media.raw.len(),
            Self::Metadata(metadata) => metadata.raw.len(),
            Self::Script { payload, .. } => payload.len(),
            Self::End(_) | Self::Failed(_) => 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IngressSendError {
    #[error("one RTMP message requires {required} bytes, above the queue capacity of {capacity}")]
    TooLarge { required: usize, capacity: usize },
    #[error("the RTMP packet source has gone away")]
    ReaderGone,
    #[error("the RTMP ingress queue is already terminal")]
    Terminal,
}

enum Terminal {
    End(InputState),
    Failed(Box<str>),
}

struct State {
    events: VecDeque<IngressEvent>,
    queued_bytes: usize,
    reader_alive: bool,
    terminal: Option<Terminal>,
}

struct Shared {
    capacity: usize,
    state: Mutex<State>,
    readable: Notify,
    writable: Notify,
}

pub struct IngressReader {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct IngressWriter {
    shared: Arc<Shared>,
}

pub fn channel(capacity: NonZeroUsize) -> (IngressReader, IngressWriter) {
    let shared = Arc::new(Shared {
        capacity: capacity.get(),
        state: Mutex::new(State {
            events: VecDeque::new(),
            queued_bytes: 0,
            reader_alive: true,
            terminal: None,
        }),
        readable: Notify::new(),
        writable: Notify::new(),
    });
    (
        IngressReader {
            shared: Arc::clone(&shared),
        },
        IngressWriter { shared },
    )
}

impl IngressWriter {
    pub async fn send(&self, event: IngressEvent) -> Result<(), IngressSendError> {
        let required = event.queued_bytes();
        if required > self.shared.capacity {
            return Err(IngressSendError::TooLarge {
                required,
                capacity: self.shared.capacity,
            });
        }
        loop {
            let notified = self.shared.writable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.shared.state.lock();
                if !state.reader_alive {
                    return Err(IngressSendError::ReaderGone);
                }
                if state.terminal.is_some() {
                    return Err(IngressSendError::Terminal);
                }
                if state.queued_bytes <= self.shared.capacity - required {
                    state.queued_bytes += required;
                    state.events.push_back(event);
                    self.shared.readable.notify_one();
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub fn finish(&self, state: InputState) {
        debug_assert!(!state.is_open(), "a terminal ingress cannot remain open");
        self.set_terminal(Terminal::End(state));
    }

    pub fn fail(&self, error: impl Into<Box<str>>) {
        self.set_terminal(Terminal::Failed(error.into()));
    }

    fn set_terminal(&self, terminal: Terminal) {
        let mut state = self.shared.state.lock();
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
            self.shared.readable.notify_waiters();
            self.shared.writable.notify_waiters();
        }
    }
}

impl IngressReader {
    pub async fn recv(&mut self) -> IngressEvent {
        loop {
            if let Some(event) = self.try_recv() {
                return event;
            }
            self.shared.readable.notified().await;
        }
    }

    pub fn try_recv(&mut self) -> Option<IngressEvent> {
        let mut state = self.shared.state.lock();
        if let Some(event) = state.events.pop_front() {
            state.queued_bytes -= event.queued_bytes();
            drop(state);
            self.shared.writable.notify_waiters();
            return Some(event);
        }
        match state.terminal.take() {
            Some(Terminal::End(state)) => Some(IngressEvent::End(state)),
            Some(Terminal::Failed(error)) => Some(IngressEvent::Failed(error)),
            None => None,
        }
    }
}

impl Drop for IngressReader {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.reader_alive = false;
        state.events.clear();
        state.queued_bytes = 0;
        drop(state);
        self.shared.writable.notify_waiters();
    }
}
