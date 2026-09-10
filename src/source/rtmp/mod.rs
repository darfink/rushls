//! Native RTMP `PacketSource` over `rtmpx` validated media.
//!
//! The transport owns the socket and handshake. This module maps parsed audio
//! and video messages onto the same [`Packet`](crate::source::Packet) contract
//! MPEG-TS uses, including length-prefixed video and raw AAC.

mod caption;
mod map;
mod metadata;
mod queue;
mod source;

#[cfg(test)]
pub(crate) use caption::encode_cue;
pub use queue::{IngressEvent, IngressReader, IngressSendError, IngressWriter, channel};
pub use source::RtmpPacketSource;
