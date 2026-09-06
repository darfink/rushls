//! The retained media a playlist projection reads.
//!
//! Everything here is immutable once published. A reader holds an [`Arc`] to a
//! [`RenditionSnapshot`] and can take as long as it likes over it: the writer
//! that advances the rendition replaces the handle rather than mutating what
//! the reader already has.

use std::sync::Arc;

use crate::domain::{Payload, RenditionId, TickDuration, TickTimestamp, Timebase};
use crate::mux::{PackagingSegmentId, RenditionConfig};
use derive_more::Deref;

use super::disk::HeldBytes;
use super::{InitializationId, Msn, PartCursor, PartId, PlaylistContract, SegmentId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredInitialization {
    /// Unique within this rendition for the lifetime of the logical stream.
    pub id: InitializationId,
    pub version: u64,
    pub payload: Payload,
    /// The gzip encoding of this resource, for the text formats HLS asks
    /// servers to transfer compressed.
    ///
    /// Computed once by the publisher rather than per request, and held here
    /// opaquely: the store never inspects it and does not know which formats
    /// are text.
    pub gzip: Option<Payload>,
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
    pub payload: HeldBytes,
    /// The gzip encoding of this resource, for the text formats HLS asks
    /// servers to transfer compressed.
    ///
    /// Computed once by the publisher rather than per request, and held here
    /// opaquely: the store never inspects it and does not know which formats
    /// are text.
    pub gzip: Option<HeldBytes>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentBody {
    Contiguous(HeldBytes),
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
    /// Whether a projection must precede this segment with a discontinuity.
    ///
    /// Decided when the parent segment is *created*, not when it completes,
    /// and against the previous parent rather than the previous retained
    /// segment: the segment a reconnect follows may already have been evicted,
    /// and a discontinuity that vanished with it would splice two publications
    /// together silently.
    pub discontinuity_before: bool,
    pub kind: StoredSegmentKind,
    /// The gzip encoding of this resource, for the text formats HLS asks
    /// servers to transfer compressed.
    ///
    /// Computed once by the publisher rather than per request, and held here
    /// opaquely: the store never inspects it and does not know which formats
    /// are text.
    pub gzip: Option<HeldBytes>,
}

/// Completed segments and the prefix frontier controlling their PART tags.
///
/// Part retention hides whole completed parents from oldest to newest. A gap
/// may sit beyond the frontier after an interrupted open segment, but its kind
/// carries no part body, so it still cannot expose the retained payloads it
/// replaced.
#[derive(Clone, Debug, Default, Deref, Eq, PartialEq)]
pub struct PublishedSegments {
    #[deref]
    entries: Arc<[Arc<StoredSegment>]>,
    parts_visible_from: Option<Msn>,
}

impl PublishedSegments {
    pub(crate) fn new(entries: Arc<[Arc<StoredSegment>]>, parts_visible_from: Option<Msn>) -> Self {
        Self {
            entries,
            parts_visible_from,
        }
    }

    pub fn parts<'a>(&self, segment: &'a StoredSegment) -> &'a [Arc<StoredPart>] {
        if self
            .parts_visible_from
            .is_some_and(|frontier| segment.msn >= frontier)
            && let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &segment.kind
        {
            return parts;
        }
        &[]
    }
}

/// The bytes a completed segment charges against the retention budget.
///
/// A gap has none, and a chunked body's bytes are already charged to the parts
/// that compose it, so this counts only what the segment itself holds.
/// Distinct from [`segment_byte_len`], which describes the reassembled
/// resource for bitrate accounting.
pub fn segment_resource_bytes(segment: &StoredSegment) -> usize {
    let payload = match &segment.kind {
        StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) => payload.memory_bytes(),
        StoredSegmentKind::Media(SegmentBody::Chunked(_)) | StoredSegmentKind::Gap => 0,
    };
    payload.saturating_add(segment.gzip.as_ref().map_or(0, HeldBytes::memory_bytes))
}

pub fn segment_disk_bytes(segment: &StoredSegment) -> usize {
    let payload = match &segment.kind {
        StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) => payload.disk_bytes(),
        StoredSegmentKind::Media(SegmentBody::Chunked(_)) | StoredSegmentKind::Gap => 0,
    };
    payload.saturating_add(segment.gzip.as_ref().map_or(0, HeldBytes::disk_bytes))
}

/// The size of the media a completed segment carries, reassembled.
///
/// A chunked segment's size is the sum of its parts. Used where the bytes a
/// viewer would receive matter (bitrate), never where retention is charged —
/// see [`segment_resource_bytes`].
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
    /// Whether a projection must precede this segment with a discontinuity.
    ///
    /// Present on the open segment because `EXT-X-DISCONTINUITY`,
    /// `EXT-X-MAP`, and `EXT-X-PROGRAM-DATE-TIME` all belong before the
    /// segment's first `EXT-X-PART` tag. Waiting for completion to learn this
    /// would publish the parts of a new publication under the previous one's
    /// timeline.
    pub discontinuity_before: bool,
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
    /// The playlist terms this rendition was created under, and keeps.
    pub contract: PlaylistContract,
    /// The `EXT-X-MEDIA-SEQUENCE` value this snapshot must advertise.
    ///
    /// Explicit rather than read from the first visible segment, because a
    /// playlist may legitimately have no visible segment — before the first
    /// one completes, or once the window has been swept — and still has to
    /// name the position its next tag will occupy.
    pub media_sequence: u64,
    /// The `EXT-X-DISCONTINUITY-SEQUENCE` value: how many discontinuity tags
    /// have already been evicted from the front of this playlist.
    pub discontinuity_sequence: u64,
    /// Every initialization still referenced by current or downloadable media.
    ///
    /// This is intentionally not just the newest header: media from the
    /// previous publisher may remain fetchable after takeover.
    pub initializations: Arc<[StoredInitialization]>,
    pub segments: PublishedSegments,
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
