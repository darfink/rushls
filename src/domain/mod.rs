//! Vocabulary shared by every layer: identities, tick arithmetic, track
//! metadata, and encoded payloads.
//!
//! This module depends on nothing else in the crate. Everything above it may
//! depend on it, which is what keeps the rest of the dependency graph acyclic.

use std::{future::Future, pin::Pin};

pub mod aac;
mod appender;
mod ids;
mod instant;
mod language;
mod payload;
mod publisher;
pub use publisher::{ClientInfo, IngestProtocol, PublishResource, PublisherContext};
mod rfc6381;
mod time;
mod track;

#[cfg(test)]
pub mod fixtures;

pub use appender::Appender;
pub use ids::{RenditionId, SessionId, SourceTrackKey, StreamId, TrackId};
pub use instant::MediaInstant;
pub use payload::Payload;
pub use rfc6381::rfc6381;
pub use time::{
    RationalTickAccumulator, TickDuration, TickOffset, TickTimestamp, Timebase, TimebaseProjection,
    duration_from_nanos_saturating, duration_since, offset_from,
};
pub use track::{
    AudioTiming, AudioTrim, Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters,
    SubtitlePosition, TrackCatalog, TrackCatalogError, TrackCounts, WebVttCueMetadata,
};

/// A boxed future returned by object-safe trait methods.
///
/// Used deliberately and only where a single allocation is amortized away:
/// once per session for handshakes and planning, once per packet *batch* on the
/// streaming path. No trait in this crate boxes a future per packet or sample.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
