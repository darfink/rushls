//! FFmpeg AVFormat demuxing behind a safe, bounded `PacketSource`.
//!
//! Raw FFmpeg types are confined to `ffi` and `metadata`. The session-facing
//! adapter exchanges Rust-owned packets with a dedicated blocking worker, so
//! neither Tokio nor the rest of the media pipeline can observe an FFmpeg
//! pointer or depend on its ownership rules.

mod control;
mod ffi;
#[cfg(test)]
pub(crate) mod fixtures;
mod input;
mod metadata;
mod source;
mod worker;

pub use input::{AvformatInput, AvformatInputError, AvformatInterrupt, ReadInput};
pub use source::{AvformatConfig, AvformatPacketSource};
