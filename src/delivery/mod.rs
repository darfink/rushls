//! Making muxed media fetchable.
//!
//! Delivery owns playlists, retention, and the handoff to whatever serves
//! HTTP. It consumes [`PackagedMedia`](crate::mux::PackagedMedia) and knows
//! nothing about how those bytes were produced.

pub mod hls;
