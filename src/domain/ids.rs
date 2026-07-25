use std::num::NonZeroU64;

use derive_more::Display;

/// Identifies a track within one publication, stable from discovery onward.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("track/{_0}")]
pub struct TrackId(pub u32);

/// Identifies a delivery rendition, which one or more tracks are muxed into.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("rendition/{_0}")]
pub struct RenditionId(pub u32);

/// Identifies one ingest session for its lifetime.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("session/{_0}")]
pub struct SessionId(pub NonZeroU64);

/// Names the logical stream a publication owns and viewers request.
///
/// Two sessions may hold the same stream identity in sequence, never at once:
/// admission rejects the newcomer or the registry displaces the incumbent.
#[derive(Clone, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamId(pub String);

impl StreamId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
