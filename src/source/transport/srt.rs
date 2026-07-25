//! SRT handshake and MPEG-TS packet source.
//!
//! The concrete implementation will parse the SRT stream ID into a generic
//! publish request and yield a [`PacketSource`](crate::source::PacketSource)
//! over the transport-stream multiplex, reporting SRT loss statistics through
//! its [`SourceMeters`](crate::observe::SourceMeters).
