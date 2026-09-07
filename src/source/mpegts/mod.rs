//! Streaming MPEG-TS demux behind a bounded `PacketSource`.
//!
//! [`StreamingTsDemux`](transmux::StreamingTsDemux) consumes encoded bytes on a
//! Tokio task so SRT receives stay on the same runtime as the driver. The
//! adapter maps resolved tracks and access units onto the same [`Packet`]
//! contract RTMP uses, including Annex-B → length-prefixed video and ADTS →
//! raw AAC.

mod av1;
mod control;
mod map;
mod opus;
mod source;
mod worker;

pub use source::{MpegTsConfig, MpegTsPacketSource};
