use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc, time::Duration};

use bytes::{Buf, Bytes};
use parking_lot::{Condvar, Mutex};
use thiserror::Error;
use tokio::sync::Notify;

use crate::source::InputState;

use super::{AvformatInput, AvformatInputError, AvformatInterrupt};

const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ByteChannelWriteError {
    #[error("one byte group requires {required} bytes, above the channel capacity of {capacity}")]
    GroupTooLarge { required: usize, capacity: usize },
    #[error("the AVFormat byte reader has gone away")]
    ReaderGone,
    #[error("the AVFormat byte channel is already terminal")]
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Terminal {
    End(InputState),
    Failed(Box<str>),
}

#[derive(Debug)]
struct State {
    chunks: VecDeque<Bytes>,
    queued_bytes: usize,
    reader_alive: bool,
    terminal: Option<Terminal>,
}

#[derive(Debug)]
struct Shared {
    capacity: usize,
    state: Mutex<State>,
    readable: Condvar,
    writable: Notify,
}

/// The blocking half of a byte-bounded async-to-AVFormat bridge.
///
/// The channel bounds actual retained bytes rather than message count. This is
/// important for live ingest because RTMP messages can vary from a few bytes to
/// the FLV format's 16 MiB limit.
pub struct AvformatByteChannel {
    shared: Arc<Shared>,
    current: Bytes,
}

/// The asynchronous half of an [`AvformatByteChannel`].
#[derive(Clone, Debug)]
pub struct AvformatByteChannelWriter {
    shared: Arc<Shared>,
}

impl AvformatByteChannel {
    pub fn new(capacity: NonZeroUsize) -> (Self, AvformatByteChannelWriter) {
        let shared = Arc::new(Shared {
            capacity: capacity.get(),
            state: Mutex::new(State {
                chunks: VecDeque::new(),
                queued_bytes: 0,
                reader_alive: true,
                terminal: None,
            }),
            readable: Condvar::new(),
            writable: Notify::new(),
        });
        (
            Self {
                shared: Arc::clone(&shared),
                current: Bytes::new(),
            },
            AvformatByteChannelWriter { shared },
        )
    }
}

impl AvformatByteChannelWriter {
    /// Queues one indivisible byte group.
    ///
    /// All pieces become visible together, which lets transports enqueue a
    /// header, borrowed payload, and footer without exposing a partial frame if
    /// the task is cancelled while waiting for capacity.
    pub async fn send_group<const N: usize>(
        &self,
        chunks: [Bytes; N],
    ) -> Result<(), ByteChannelWriteError> {
        let required = chunks
            .iter()
            .try_fold(0_usize, |total, chunk| total.checked_add(chunk.len()))
            .unwrap_or(usize::MAX);
        if required > self.shared.capacity {
            return Err(ByteChannelWriteError::GroupTooLarge {
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
                    return Err(ByteChannelWriteError::ReaderGone);
                }
                if state.terminal.is_some() {
                    return Err(ByteChannelWriteError::Terminal);
                }
                if state.queued_bytes <= self.shared.capacity - required {
                    state.queued_bytes += required;
                    state
                        .chunks
                        .extend(chunks.into_iter().filter(|chunk| !chunk.is_empty()));
                    self.shared.readable.notify_one();
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub async fn send(&self, bytes: Bytes) -> Result<(), ByteChannelWriteError> {
        self.send_group([bytes]).await
    }

    /// Marks a clean or interrupted end after all already queued bytes.
    pub fn finish(&self, state: InputState) {
        debug_assert!(!state.is_open(), "a terminal channel cannot remain open");
        self.set_terminal(Terminal::End(state));
    }

    pub fn fail(&self, error: impl Into<Box<str>>) {
        self.set_terminal(Terminal::Failed(error.into()));
    }

    fn set_terminal(&self, terminal: Terminal) {
        let mut state = self.shared.state.lock();
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
            self.shared.readable.notify_all();
            self.shared.writable.notify_waiters();
        }
    }
}

impl AvformatInput for AvformatByteChannel {
    fn read(
        &mut self,
        buffer: &mut [u8],
        interrupt: &dyn AvformatInterrupt,
    ) -> Result<usize, AvformatInputError> {
        if buffer.is_empty() {
            return Ok(0);
        }

        loop {
            if interrupt.interrupted() {
                return Err(AvformatInputError::End(InputState::Interrupted));
            }
            if !self.current.is_empty() {
                let read = buffer.len().min(self.current.len());
                buffer[..read].copy_from_slice(&self.current[..read]);
                self.current.advance(read);
                let mut state = self.shared.state.lock();
                state.queued_bytes -= read;
                drop(state);
                self.shared.writable.notify_waiters();
                return Ok(read);
            }

            let mut state = self.shared.state.lock();
            if let Some(chunk) = state.chunks.pop_front() {
                self.current = chunk;
                continue;
            }
            if let Some(terminal) = &state.terminal {
                return match terminal {
                    Terminal::End(state) => Err(AvformatInputError::End(*state)),
                    Terminal::Failed(error) => Err(AvformatInputError::Failed(error.clone())),
                };
            }

            self.shared
                .readable
                .wait_for(&mut state, INTERRUPT_POLL_INTERVAL);
        }
    }
}

impl Drop for AvformatByteChannel {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.reader_alive = false;
        state.chunks.clear();
        state.queued_bytes = 0;
        drop(state);
        self.shared.writable.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct Interrupt(AtomicBool);

    impl AvformatInterrupt for Interrupt {
        fn interrupted(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[tokio::test]
    async fn preserves_groups_and_terminal_state_across_partial_reads() {
        let (mut input, writer) = AvformatByteChannel::new(nz::usize!(8));
        writer
            .send_group([Bytes::from_static(b"abc"), Bytes::from_static(b"def")])
            .await
            .expect("group fits");
        writer.finish(InputState::Closed);
        let interrupt = Interrupt(AtomicBool::new(false));
        let mut output = [0; 4];

        assert_eq!(input.read(&mut output, &interrupt), Ok(3));
        assert_eq!(&output[..3], b"abc");
        assert_eq!(input.read(&mut output, &interrupt), Ok(3));
        assert_eq!(&output[..3], b"def");
        assert_eq!(
            input.read(&mut output, &interrupt),
            Err(AvformatInputError::End(InputState::Closed))
        );
    }

    #[tokio::test]
    async fn applies_backpressure_to_whole_groups() {
        let (mut input, writer) = AvformatByteChannel::new(nz::usize!(4));
        writer
            .send(Bytes::from_static(b"full"))
            .await
            .expect("first send fits");
        let blocked = tokio::spawn({
            let writer = writer.clone();
            async move { writer.send(Bytes::from_static(b"next")).await }
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        let mut output = [0; 2];
        input
            .read(&mut output, &Interrupt(AtomicBool::new(false)))
            .expect("partial read succeeds");
        tokio::task::yield_now().await;
        assert!(
            !blocked.is_finished(),
            "bytes retained by a partial read still consume the budget"
        );
        input
            .read(&mut output, &Interrupt(AtomicBool::new(false)))
            .expect("remaining read releases capacity");
        assert_eq!(blocked.await.expect("sender did not panic"), Ok(()));
    }

    #[test]
    fn an_idle_reader_observes_interrupts() {
        let (mut input, _writer) = AvformatByteChannel::new(nz::usize!(1));
        let interrupt = Interrupt(AtomicBool::new(true));

        assert_eq!(
            input.read(&mut [0], &interrupt),
            Err(AvformatInputError::End(InputState::Interrupted))
        );
    }

    #[tokio::test]
    async fn dropping_the_reader_releases_a_blocked_writer() {
        let (input, writer) = AvformatByteChannel::new(nz::usize!(1));
        writer
            .send(Bytes::from_static(b"a"))
            .await
            .expect("first send fits");
        let blocked = tokio::spawn({
            let writer = writer.clone();
            async move { writer.send(Bytes::from_static(b"b")).await }
        });
        tokio::task::yield_now().await;
        drop(input);

        assert_eq!(
            blocked.await.expect("sender did not panic"),
            Err(ByteChannelWriteError::ReaderGone)
        );
    }
}
