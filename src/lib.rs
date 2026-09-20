//! A low-latency HLS origin: ingest a live publication, plan it, mux it, serve it.
//!
//! # Layers
//!
//! Modules are layered, and the dependency graph is acyclic by construction.
//! Each layer may use anything below it and nothing above it:
//!
//! ```text
//! server      process wiring, operator surface
//! session     lifecycle, registry, health, supervision
//! delivery    playlists, retention, the stream store viewers read
//! mux         container encapsulation, segment cutting
//! segment     segmentation policy, boundary discovery, pre-roll
//! media       validation, timeline calibration, normalization
//! source      transports and demux, fused: a protocol implies its container
//! hooks       lifecycle delivery to operator-configured endpoints
//! admission   who may publish what
//! observe     meters and lifecycle events
//! outbound    requests to operator-configured services
//! domain      identities, tick arithmetic, tracks, payloads
//! ```
//!
//! `outbound` sits near the bottom because both `admission` and `observe` call
//! out to endpoints an administrator configured, and neither may depend on the
//! other. It knows nothing about publishers, streams, or events.
//!
//! # The streaming path
//!
//! Every stage between the socket and the playlist appends into a caller-owned
//! buffer that is reused across iterations:
//!
//! ```text
//! source.fill      -> Vec<Packet>          one await per batch
//! normalizer.push  -> Vec<NormalizedMedia>
//! muxer.push       -> Vec<PackagedMedia>
//! publisher.write  -> StreamStore
//! ```
//!
//! That keeps every stage object-safe, so no type parameter escapes the hot
//! path into the code that assembles a session, and it keeps errors flat
//! instead of nesting each stage's failure inside the next one's.

pub mod admission;
pub mod delivery;
pub mod domain;
pub mod hooks;
pub mod media;
pub mod mux;
pub mod observe;
pub mod outbound;
pub mod segment;
pub mod server;
pub mod session;
pub mod source;

#[cfg(all(test, feature = "allocation-counting"))]
pub mod test_alloc;

#[cfg(test)]
pub(crate) mod test_fuzz;

/// Package version and the git commit this binary was built from.
///
/// Set at compile time from `CARGO_PKG_VERSION` and `GIT_SHA` (see `build.rs`).
/// Used for `--version` and the startup banner.
pub fn version() -> &'static str {
    concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_SHA"), ")")
}
