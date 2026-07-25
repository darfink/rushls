use std::io::Read;

use crate::source::InputState;

/// Why a byte input could not satisfy another read.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AvformatInputError {
    #[error("the byte input ended: {0:?}")]
    End(InputState),
    #[error("byte input failed: {0}")]
    Failed(String),
}

/// Cancellation and discovery-deadline state shared with a blocking reader.
pub trait AvformatInterrupt {
    fn interrupted(&self) -> bool;
}

/// Blocking byte stream consumed exclusively by the AVFormat worker.
///
/// The AVFormat worker drives this method whenever its custom AVIO buffer needs
/// more encoded bytes. Live transports should bridge async socket reads through
/// a bounded byte channel; while that channel is idle, its blocking receiver
/// must also observe `interrupt`. FFmpeg cannot invoke its own interrupt
/// callback while execution is inside this Rust method.
pub trait AvformatInput: Send {
    fn read(
        &mut self,
        buffer: &mut [u8],
        interrupt: &dyn AvformatInterrupt,
    ) -> Result<usize, AvformatInputError>;
}

/// Adapts an ordinary blocking reader into an AVFormat byte input.
///
/// Intended for finite memory/file readers. `std::io::Read` has no cancellation
/// facility, so live sockets should implement [`AvformatInput`] directly and
/// observe [`AvformatInterrupt`].
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

impl<R: Read + Send> AvformatInput for ReadInput<R> {
    fn read(
        &mut self,
        buffer: &mut [u8],
        _interrupt: &dyn AvformatInterrupt,
    ) -> Result<usize, AvformatInputError> {
        match self.reader.read(buffer) {
            Ok(0) => Err(AvformatInputError::End(self.end)),
            Ok(read) => Ok(read),
            Err(error) => Err(AvformatInputError::Failed(error.to_string())),
        }
    }
}
