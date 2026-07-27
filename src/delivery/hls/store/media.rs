//! The retained media a playlist projection reads.
//!
//! Everything here is immutable once published. A reader holds an [`Arc`] to a
//! [`RenditionSnapshot`] and can take as long as it likes over it: the writer
//! that advances the rendition replaces the handle rather than mutating what
//! the reader already has.

use std::sync::Arc;

use crate::domain::{Payload, RenditionId, TickDuration, TickTimestamp, Timebase};
use crate::mux::{PackagingSegmentId, RenditionConfig};

use super::{InitializationId, Msn, PartCursor, PartId, SegmentId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredInitialization {
    /// Unique within this rendition for the lifetime of the logical stream.
    pub id: InitializationId,
    pub version: u64,
    pub payload: Payload,
}

/// A partial segment retained for playlist and standalone-resource delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPart {
    pub id: PartId,
    pub cursor: PartCursor,
    /// Publisher generation that produced the media. A projection emits a
    /// discontinuity when this changes between neighbouring segments.
    pub publication: u64,
    /// Initialization section required to decode this part.
    pub initialization: InitializationId,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub timebase: Timebase,
    pub independent: bool,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentBody {
    Contiguous(Payload),
    Chunked(Arc<[Arc<StoredPart>]>),
}

impl SegmentBody {
    pub fn len(&self) -> usize {
        match self {
            Self::Contiguous(payload) => payload.len(),
            Self::Chunked(parts) => parts.iter().map(|part| part.payload.len()).sum(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoredSegmentKind {
    Media(SegmentBody),
    Gap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSegment {
    pub id: SegmentId,
    pub msn: Msn,
    /// Publisher generation that produced the media.
    pub publication: u64,
    /// Initialization section required to decode this segment.
    pub initialization: InitializationId,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub timebase: Timebase,
    pub independent: bool,
    /// Parts still eligible to appear as EXT-X-PART tags.
    pub parts: Vec<Arc<StoredPart>>,
    pub kind: StoredSegmentKind,
}

/// The bytes a completed segment charges against the retention budget.
///
/// A gap has none, and a chunked body's bytes are already charged to the parts
/// that compose it, so this counts only what the segment itself holds.
pub fn segment_byte_len(segment: &StoredSegment) -> usize {
    match &segment.kind {
        StoredSegmentKind::Media(body) => body.len(),
        StoredSegmentKind::Gap => 0,
    }
}

/// The one progressively published segment a chunked rendition may have open.
///
/// Both the writer's working state and the published view: a reader needs the
/// accumulated `duration` to place the next part, and keeping one type removes
/// a field-by-field conversion whose only job was to hide it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSegment {
    pub id: SegmentId,
    pub msn: Msn,
    pub publication: u64,
    pub initialization: InitializationId,
    pub packaging_segment_id: PackagingSegmentId,
    pub media_start: TickTimestamp,
    /// Media accumulated by the parts published so far.
    pub duration: TickDuration,
    pub parts: Vec<Arc<StoredPart>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenditionLiveEdge {
    pub last_segment: Option<(Msn, SegmentId)>,
    pub last_part: Option<(PartCursor, PartId)>,
    pub next_part_id: Option<PartId>,
    pub ended: bool,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenditionBitrateStatistics {
    pub peak_bits_per_second: Option<u64>,
    pub average_bits_per_second: Option<u64>,
    pub observed_segments: u64,
}

impl RenditionBitrateStatistics {
    /// The subset a multivariant projection actually advertises.
    ///
    /// Comparing this rather than the whole record is what keeps a manifest
    /// from being invalidated by an observation count nobody publishes.
    pub fn advertised(self) -> RenditionBandwidth {
        RenditionBandwidth {
            peak_bits_per_second: self.peak_bits_per_second,
            average_bits_per_second: self.average_bits_per_second,
        }
    }
}

/// Bitrate attributes consumed by a multivariant playlist projection.
///
/// Observation counts remain in [`RenditionBitrateStatistics`] for diagnostics
/// but do not invalidate a manifest when the advertised rates are unchanged.
/// A rolling average may still change once per completed segment; the eventual
/// projection should apply its own rounding or hysteresis before replacing a
/// cached rendered manifest. Keeping exact values here lets that policy evolve
/// without discarding authoritative observations in the store.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenditionBandwidth {
    pub peak_bits_per_second: Option<u64>,
    pub average_bits_per_second: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenditionSnapshot {
    pub rendition_id: RenditionId,
    pub config: Option<RenditionConfig>,
    /// Every initialization still referenced by current or downloadable media.
    ///
    /// This is intentionally not just the newest header: media from the
    /// previous publisher may remain fetchable after takeover.
    pub initializations: Vec<StoredInitialization>,
    pub segments: Vec<StoredSegment>,
    pub open_segment: Option<OpenSegment>,
    pub live_edge: RenditionLiveEdge,
    pub bitrate: RenditionBitrateStatistics,
}

impl RenditionSnapshot {
    /// Whether the snapshot contains any completed parent segment.
    ///
    /// HLS permits an empty Media Playlist, so this is an operational readiness
    /// signal rather than a syntax-validity check. HTTP delivery may choose to
    /// wait for a completed segment to give a newly joining player a useful
    /// starting window.
    pub fn has_completed_segment(&self) -> bool {
        !self.segments.is_empty()
    }

    pub fn initialization_for(
        &self,
        initialization: InitializationId,
    ) -> Option<&StoredInitialization> {
        self.initializations
            .iter()
            .find(|held| held.id == initialization)
    }
}
