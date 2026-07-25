//! RTMP handshake and FLV packet source.
//!
//! The concrete implementation will terminate RTMP protocol concerns here and
//! yield a [`PacketSource`](crate::source::PacketSource) over the FLV-framed
//! elementary streams the protocol carries.
