use std::{num::NonZeroU64, sync::Arc};

use derive_more::Display;

/// Identifies a track within one publication, stable from discovery onward.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("track/{_0}")]
pub struct TrackId(pub u32);

/// Opaque source-provided identity for matching a track across publications.
///
/// Enhanced RTMP track IDs and container-level identities such as a Matroska
/// TrackUID can populate this without teaching media or muxing about a
/// transport. Absence is meaningful: a local [`TrackId`] is deterministic
/// within one publication but is not promised to survive reconnects.
#[derive(Clone, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceTrackKey(pub Arc<str>);

impl SourceTrackKey {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }
}

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
