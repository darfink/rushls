//! Vocabulary shared by every layer: identities, tick arithmetic, track
//! metadata, and encoded payloads.
//!
//! This module depends on nothing else in the crate. Everything above it may
//! depend on it, which is what keeps the rest of the dependency graph acyclic.

use std::{future::Future, pin::Pin};

mod appender;
mod ids;
mod payload;
mod time;
mod track;

pub use appender::Appender;
pub use ids::{RenditionId, SessionId, SourceTrackKey, StreamId, TrackId};
pub use payload::Payload;
pub use time::{TickDuration, TickOffset, TickTimestamp, Timebase, duration_since, offset_from};
pub use track::{
    Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters, TrackCatalog, TrackCatalogError,
    TrackCounts,
};

/// A boxed future returned by object-safe trait methods.
///
/// Used deliberately and only where a single allocation is amortized away:
/// once per session for handshakes and planning, once per packet *batch* on the
/// streaming path. No trait in this crate boxes a future per packet or sample.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
