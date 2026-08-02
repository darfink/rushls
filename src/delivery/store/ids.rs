//! Durable delivery identities.
//!
//! Every identity here is assigned by the store and survives publisher
//! reconnects, which is what separates them from the packaging-local
//! identities in [`mux`](crate::mux). A publisher that restarts its own
//! numbering does not disturb any of these.

use derive_more::Display;

/// Identifies one initialization section within a rendition, for its lifetime.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct InitializationId(pub u64);

/// Identifies one retained segment resource within a rendition.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct SegmentId(pub u64);

/// Identifies one retained partial-segment resource within a rendition.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct PartId(pub u64);

/// An HLS Media Sequence Number.
///
/// Distinct from [`SegmentId`]: MSN is the playlist's monotonic position, while
/// the segment ID names a fetchable resource that may outlive its tag.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct Msn(pub u64);

/// A part's position within its parent segment.
#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct PartIndex(pub u32);

/// The `(MSN, part index)` pair an LL-HLS blocking reload names.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PartCursor {
    pub msn: Msn,
    pub part_index: PartIndex,
}
