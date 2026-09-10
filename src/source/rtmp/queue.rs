//! Bounded async queue from the RTMP session onto the packet source.
//!
//! The session is already on Tokio, so this is a byte-budgeted channel rather
//! than a blocking worker. Byte and event-count limits make the publisher
//! wait when the pipeline stops draining.

use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use parking_lot::Mutex;
use rtmpx::{ParsedAudio, ParsedVideo, ValidatedMedia, ValidatedMetadata};
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
    pub fn queued_bytes(&self) -> usize {
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

#[derive(Clone)]
enum Terminal {
    End(InputState),
    Failed(Box<str>),
}

struct State {
    events: VecDeque<IngressEvent>,
    queued_bytes: usize,
    reader_alive: bool,
    writers: usize,
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
            writers: 1,
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

impl Clone for IngressWriter {
    fn clone(&self) -> Self {
        self.shared.state.lock().writers += 1;
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for IngressWriter {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.writers -= 1;
        if state.writers == 0 && state.terminal.is_none() {
            state.terminal = Some(Terminal::End(InputState::Interrupted));
            self.shared.readable.notify_waiters();
        }
    }
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
                // A byte limit alone permits unbounded empty/tiny script tags.
                if state.events.len() < 4_096
                    && state.queued_bytes <= self.shared.capacity - required
                {
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
            let notified = self.shared.readable.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(event) = self.try_recv() {
                return event;
            }
            notified.await;
        }
    }

    pub fn try_recv(&self) -> Option<IngressEvent> {
        let mut state = self.shared.state.lock();
        if let Some(event) = state.events.pop_front() {
            state.queued_bytes -= event.queued_bytes();
            drop(state);
            self.shared.writable.notify_waiters();
            return Some(event);
        }
        match state.terminal.clone() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn last_writer_drop_interrupts_the_reader() {
        let (mut reader, writer) = channel(nz::usize!(1));
        let other = writer.clone();
        drop(writer);
        assert!(reader.try_recv().is_none());
        drop(other);
        assert!(matches!(
            reader.recv().await,
            IngressEvent::End(InputState::Interrupted)
        ));
    }

    #[tokio::test]
    async fn terminal_is_sticky_and_wakes_a_blocked_sender() -> Result<(), IngressSendError> {
        let (mut reader, writer) = channel(nz::usize!(1));
        writer
            .send(IngressEvent::Script {
                timestamp: 0,
                payload: Bytes::from_static(b"x"),
            })
            .await?;
        let next = writer.send(IngressEvent::Script {
            timestamp: 1,
            payload: Bytes::from_static(b"y"),
        });
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut next)
                .await
                .is_err()
        );
        writer.finish(InputState::Closed);
        assert_eq!(next.await, Err(IngressSendError::Terminal));
        assert!(matches!(reader.recv().await, IngressEvent::Script { .. }));
        assert!(matches!(
            reader.recv().await,
            IngressEvent::End(InputState::Closed)
        ));
        assert!(matches!(
            reader.recv().await,
            IngressEvent::End(InputState::Closed)
        ));
        assert_eq!(
            writer.send(IngressEvent::End(InputState::Closed)).await,
            Err(IngressSendError::Terminal)
        );
        Ok(())
    }

    #[tokio::test]
    async fn finish_wakes_an_empty_reader() {
        let (mut reader, writer) = channel(nz::usize!(1));
        let receive = reader.recv();
        tokio::pin!(receive);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut receive)
                .await
                .is_err()
        );
        writer.finish(InputState::Closed);
        assert!(matches!(
            receive.await,
            IngressEvent::End(InputState::Closed)
        ));
    }
}
