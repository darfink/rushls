//! Making muxed media fetchable.
//!
//! Delivery owns playlists, retention, and the handoff to whatever serves
//! HTTP. It consumes [`MuxedMedia`](crate::mux::MuxedMedia) and knows nothing
//! about how those bytes were produced.

pub mod hls;
