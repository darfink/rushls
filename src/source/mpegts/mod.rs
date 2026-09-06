//! Streaming MPEG-TS demux behind a bounded `PacketSource`.
//!
//! [`StreamingTsDemux`](transmux::StreamingTsDemux) consumes encoded bytes on a
//! dedicated worker so blocking SRT receives never sit on a Tokio worker. The
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
