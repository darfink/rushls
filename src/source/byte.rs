//! Encoded-byte inputs shared by demux adapters.
//!
//! Live transports yield bytes as they arrive. Demuxers consume those bytes
//! through this contract so SRT can feed MPEG-TS without the packet source
//! knowing about sockets. Reads are async: the MPEG-TS demux task sits on the
//! same Tokio runtime as the SRT driver, and a cancelled session aborts a
//! pending receive by dropping it.

use std::io::Read;

use crate::{domain::BoxFuture, source::InputState};

/// Why a byte input could not satisfy another read.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ByteInputError {
    #[error("the byte input ended: {0:?}")]
    End(InputState),
    #[error("byte input failed: {0}")]
    Failed(Box<str>),
}

/// Async byte stream consumed exclusively by a demux task.
///
/// The task drives this method whenever it needs more encoded bytes. A live
/// socket should complete when the peer shuts down or the connection breaks;
/// cancellation is the demux task's job, by dropping the read future.
pub trait ByteInput: Send {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8])
    -> BoxFuture<'a, Result<usize, ByteInputError>>;
}

/// Adapts an ordinary blocking reader into a demux byte input.
///
/// Intended for finite memory/file readers. `std::io::Read` has no cancellation
/// facility; live sockets implement [`ByteInput`] directly.
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
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
    ) -> BoxFuture<'a, Result<usize, ByteInputError>> {
        Box::pin(async move {
            match self.reader.read(buffer) {
                Ok(0) => Err(ByteInputError::End(self.end)),
                Ok(read) => Ok(read),
                Err(error) => Err(ByteInputError::Failed(error.to_string().into())),
            }
        })
    }
}
