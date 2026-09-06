//! Blocking encoded-byte inputs shared by demux adapters.
//!
//! Live transports read sockets on their own threads. Demuxers consume those
//! bytes through this contract so SRT can feed MPEG-TS without the packet
//! source knowing about sockets.

use std::io::Read;

use crate::source::InputState;

/// Why a byte input could not satisfy another read.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ByteInputError {
    #[error("the byte input ended: {0:?}")]
    End(InputState),
    #[error("byte input failed: {0}")]
    Failed(Box<str>),
}

/// Cancellation and discovery-deadline state shared with a blocking reader.
pub trait ByteInterrupt {
    fn interrupted(&self) -> bool;
}

/// Blocking byte stream consumed exclusively by a demux worker.
///
/// The worker drives this method whenever it needs more encoded bytes. Live
/// transports should observe `interrupt` while a socket read is idle: a
/// cancelled session must not sit in a blocking receive until the peer hangs
/// up.
pub trait ByteInput: Send {
    fn read(
        &mut self,
        buffer: &mut [u8],
        interrupt: &dyn ByteInterrupt,
    ) -> Result<usize, ByteInputError>;
}

/// Adapts an ordinary blocking reader into a demux byte input.
///
/// Intended for finite memory/file readers. `std::io::Read` has no cancellation
/// facility, so live sockets should implement [`ByteInput`] directly and
/// observe [`ByteInterrupt`].
pub struct ReadInput<R> {
    reader: R,
    end: InputState,
}

impl<R> ReadInput<R> {
    pub fn closed(reader: R) -> Self {
        Self {
            reader,
            end: InputState::Closed,
        }
    }

    pub fn interrupted(reader: R) -> Self {
        Self {
            reader,
            end: InputState::Interrupted,
        }
    }
}

impl<R: Read + Send> ByteInput for ReadInput<R> {
    fn read(
        &mut self,
        buffer: &mut [u8],
        _interrupt: &dyn ByteInterrupt,
    ) -> Result<usize, ByteInputError> {
        match self.reader.read(buffer) {
            Ok(0) => Err(ByteInputError::End(self.end)),
            Ok(read) => Ok(read),
            Err(error) => Err(ByteInputError::Failed(error.to_string().into())),
        }
    }
}
