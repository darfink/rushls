//! One durable HLS rendition: what it retains, and how it advances.
//!
//! A rendition owns three overlapping lifetimes, which is most of why this is
//! the largest piece of the store:
//!
//! - **Playlist visibility** — what a media playlist may currently name.
//! - **Resource availability** — what a URL may still serve after its tag is
//!   gone, on the deadlines HLS defines.
//! - **Decodability** — which initialization sections the above still need.
//!
//! They expire independently and in that order, so a part can be untagged but
//! fetchable, and an initialization can outlive every tag that named it.
//! [`RetentionPolicy`] resolves all three; nothing here invents a deadline.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use tokio::{sync::watch, time::Instant};

use crate::{
    domain::{Payload, RenditionId},
    mux::{
        InitializationSegment, PackagedChunk, PackagedMedia, PackagedRendition, PackagedSegment,
        PackagedSegmentCompletion, PackagingSegmentId, RenditionConfig,
    },
};

use super::{
    InitializationId, MINIMUM_PLAYLIST_SEGMENTS, Msn, OpenSegment, PartCursor, PartId, PartIndex,
    PlaylistContract, PublishedSegments, RenditionBitrateStatistics, RenditionSnapshot,
    RenditionView, RetentionPolicy, SegmentBody, SegmentId, StoreWriteError, StoredInitialization,
    StoredPart, StoredSegment, StoredSegmentKind,
    bitrate::BitrateTracker,
    disk::{HeldBytes, SpillKind, SpillObject, SpillOutcome},
    media::{segment_byte_len, segment_disk_bytes, segment_resource_bytes},
};

/// Whether parts of a released parent keep fetch grace or leave with it.
#[derive(Clone, Copy)]
enum PartFate {
    /// Time-based `retain` trim: the URI stays fetchable until part grace.
    Grace,
    /// Byte/object pressure: the cap is only real if the payload leaves now.
    Immediate,
}

/// A committed live edge and the channel that should announce it.
///
/// Sending is deferred until the caller has released the state lock, so a woken
/// reader never blocks behind the writer that woke it.
pub type EdgeUpdate = (
    watch::Sender<super::RenditionLiveEdge>,
    super::RenditionLiveEdge,
);

/// Announces committed edges to blocked readers.
///
/// Revision-guarded rather than unconditional: several publications can be
/// committed under one lock and released in any order, and an older edge
/// overwriting a newer one would park a reader on media it can already fetch.
pub fn notify_edges(updates: impl IntoIterator<Item = EdgeUpdate>) {
    for (sender, edge) in updates {
        sender.send_if_modified(|current| {
            if current.revision >= edge.revision {
                return false;
            }
            *current = edge;
            true
        });
    }
}

/// A retained resource that may outlive the playlist tag which referenced it.
///
/// Segments and parts reach their deadlines by different rules — a segment's
/// comes from the longest playlist it appeared in, a part's from a fixed grace
/// period — but *checking* one is identical, so that is all this abstracts.
trait Retained {
    type Object;

    fn expires_at(&self) -> Option<Instant>;

    fn object(&self) -> &Arc<Self::Object>;
}

/// Clones a resource's payload handle unless its fetch deadline has passed.
///
/// Absent deadline means still visible in the playlist, which is never expired.
fn fetchable<R: Retained>(resource: Option<&R>, now: Instant) -> Option<Arc<R::Object>> {
    resource
        .filter(|resource| resource.expires_at().is_none_or(|deadline| now < deadline))
        .map(|resource| Arc::clone(resource.object()))
}

#[derive(Debug)]
pub struct SegmentResource {
    segment: Arc<StoredSegment>,
    pub visible: bool,
    /// Monotonic publication time used as the live-playlist availability
    /// deadline anchor required by HLS.
    first_published_at: Instant,
    /// Largest playlist duration observed while this segment was present.
    /// HLS defines segment availability using this historical maximum.
    longest_playlist_duration: Duration,
    expires_at: Option<Instant>,
}

impl Retained for SegmentResource {
    type Object = StoredSegment;

    fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }

    fn object(&self) -> &Arc<StoredSegment> {
        &self.segment
    }
}

#[derive(Debug)]
pub struct PartResource {
    part: Arc<StoredPart>,
    playlist_end: Duration,
    segment_target: Duration,
    playlist_visible: bool,
    expires_at: Option<Instant>,
    parent_retained: bool,
}

impl Retained for PartResource {
    type Object = StoredPart;

    fn expires_at(&self) -> Option<Instant> {
        self.expires_at
    }

    fn object(&self) -> &Arc<StoredPart> {
        &self.part
    }
}

#[derive(Debug)]
pub struct RenditionState {
    /// Stable logical identity used to reconnect packaging output to the same
    /// media-playlist projection across publisher takeovers.
    pub rendition_id: RenditionId,
    pub gaps: u64,
    pub publication_totals: super::telemetry::PublicationTotals,
    /// Muxer-authored identity and attributes from the latest compatible
    /// publication. This remains available while retired media is fetchable.
    pub descriptor: PackagedRendition,
    /// Frozen at creation and never revised. A publication that cannot satisfy
    /// it is matched to a different rendition rather than changing these terms
    /// under viewers already reading this playlist.
    pub contract: PlaylistContract,
    pub active: bool,
    /// Once removed from an active topology, this playlist remains terminal.
    /// Reusing it later would make an ENDLIST disappear for existing viewers.
    pub retired: bool,
    retirement_deadline: Option<Instant>,
    /// Last configuration advertised for this rendition. It survives a
    /// publisher so topology remains available while the stream is idle.
    pub advertised_config: Option<RenditionConfig>,
    /// Configuration admitted for the current publisher generation.
    pub active_config: Option<(u64, RenditionConfig)>,
    /// Initialization sections remain while current or retained media
    /// references them. Payload clones are refcounts, not byte copies.
    initializations: Arc<[StoredInitialization]>,
    pub current_initialization: Option<InitializationId>,
    issued_initializations: u64,
    /// Segment IDs appearing in the current playlist window, in MSN order.
    visible_segments: VecDeque<SegmentId>,
    /// Completed playlist parents rebuilt only when the visible window changes.
    published_segments: Arc<[Arc<StoredSegment>]>,
    /// Downloadable segment resources, including entries no longer visible
    /// whose HLS availability deadline has not elapsed.
    segment_resources: HashMap<SegmentId, SegmentResource>,
    /// Playlist-visible part ids, oldest first. Hidden tags leave this deque
    /// so `hide_old_parts` never rescans them; their payloads stay on the
    /// parent and in `part_resources` until the parent is released.
    part_order: VecDeque<PartId>,
    /// Parts whose parent is gone, still fetchable until their own grace.
    orphaned_parts: VecDeque<PartId>,
    /// Hidden parents still mapped until time-based fetch grace elapses.
    retired_segments: VecDeque<SegmentId>,
    part_resources: HashMap<PartId, PartResource>,
    /// How many retained objects still name each publication.
    ///
    /// A set would drop the id when the first sibling was released; a count
    /// keeps the anchor until the last one goes.
    publication_refs: HashMap<u64, usize>,
    spilling: HashMap<SegmentId, usize>,
    /// Disk parents awaiting the end of standalone-part fetch grace.
    chunked_spills: VecDeque<SegmentId>,
    /// At most one progressively published packaging segment per rendition.
    open_segment: Option<OpenSegment>,
    /// Payload bytes this rendition currently retains, kept incrementally so
    /// the write path never re-sums every retained resource.
    ///
    /// Charged when a payload is stored — initialization, part, or direct
    /// segment — and released when one is reclaimed. A chunked segment's
    /// bytes are already charged to the parts that compose it, so completing
    /// it changes nothing.
    retained_payload_bytes: usize,
    retained_disk_bytes: usize,
    /// HLS numbering is independent from publisher-local packaging IDs and is
    /// never reset when a publisher reconnects.
    next_msn: u64,
    next_iframe_msn: u64,
    /// The MSN a media playlist must currently advertise.
    media_sequence: u64,
    /// Discontinuity tags already evicted from the front of the playlist.
    discontinuity_sequence: u64,
    /// Publication of the most recently *created* parent segment.
    ///
    /// Deliberately not the previous retained segment: retention can evict the
    /// segment a reconnect follows, and the discontinuity between them must
    /// survive that. Deliberately not the current lease either, which changes
    /// at attach time whether or not media follows.
    last_parent_publication: Option<u64>,
    issued_segments: u64,
    issued_parts: u64,
    pub last_packaging_segment_id: Option<PackagingSegmentId>,
    /// Media-time position at the end of completed playlist segments.
    playlist_position: Duration,
    pub bitrate: BitrateTracker,
    /// Authoritative committed edge. The watch sender mirrors it only after
    /// the state lock is released.
    live_edge: super::RenditionLiveEdge,
    edge_updates: watch::Sender<super::RenditionLiveEdge>,
    /// Stable request-facing handle. Only this rendition's latest snapshot is
    /// replaced when its media advances.
    published: Arc<RenditionView>,
}

// hls.js 1.7.3 treats MSN zero as absent during part eviction checks. Starting
// at one is legal HLS and avoids repeated part loads without changing media time.
const INITIAL_MEDIA_SEQUENCE: u64 = 1;

impl RenditionState {
    /// `contract` is supplied rather than derived: its target duration is a
    /// presentation-wide value that one descriptor cannot know.
    pub fn new(
        rendition_id: RenditionId,
        descriptor: PackagedRendition,
        contract: PlaylistContract,
    ) -> Self {
        let live_edge = super::RenditionLiveEdge::default();
        let (edge_updates, _) = watch::channel(live_edge);
        let published = Arc::new(RenditionView::new(RenditionSnapshot {
            rendition_id,
            media_kind: descriptor.media.kind(),
            config: Some(descriptor.config),
            contract,
            media_sequence: INITIAL_MEDIA_SEQUENCE,
            discontinuity_sequence: 0,
            initializations: Arc::from([]),
            segments: PublishedSegments::default(),
            open_segment: None,
            live_edge,
            bitrate: RenditionBitrateStatistics::default(),
        }));
        Self {
            rendition_id,
            gaps: 0,
            publication_totals: super::telemetry::PublicationTotals::default(),
            advertised_config: Some(descriptor.config),
            active_config: None,
            descriptor,
            contract,
            active: false,
            retired: false,
            retirement_deadline: None,
            initializations: Arc::from([]),
            current_initialization: None,
            issued_initializations: 0,
            visible_segments: VecDeque::new(),
            published_segments: Arc::from([]),
            segment_resources: HashMap::new(),
            part_order: VecDeque::new(),
            orphaned_parts: VecDeque::new(),
            retired_segments: VecDeque::new(),
            part_resources: HashMap::new(),
            publication_refs: HashMap::new(),
            spilling: HashMap::new(),
            chunked_spills: VecDeque::new(),
            open_segment: None,
            retained_payload_bytes: 0,
            retained_disk_bytes: 0,
            next_msn: INITIAL_MEDIA_SEQUENCE,
            next_iframe_msn: 0,
            media_sequence: INITIAL_MEDIA_SEQUENCE,
            discontinuity_sequence: 0,
            last_parent_publication: None,
            issued_segments: 0,
            issued_parts: 0,
            last_packaging_segment_id: None,
            playlist_position: Duration::ZERO,
            bitrate: BitrateTracker::default(),
            live_edge,
            edge_updates,
            published,
        }
    }

    pub fn view(&self) -> &Arc<RenditionView> {
        &self.published
    }

    pub fn live_edge(&self) -> super::RenditionLiveEdge {
        self.live_edge
    }

    pub fn subscribe(&self) -> watch::Receiver<super::RenditionLiveEdge> {
        self.edge_updates.subscribe()
    }

    /// Fetches one retained segment resource, or `None` past its deadline.
    pub fn segment(&self, segment_id: SegmentId, now: Instant) -> Option<Arc<StoredSegment>> {
        fetchable(self.segment_resources.get(&segment_id), now)
    }

    /// Fetches one retained part resource, or `None` past its deadline.
    pub fn part(&self, part_id: PartId, now: Instant) -> Option<Arc<StoredPart>> {
        fetchable(self.part_resources.get(&part_id), now)
    }

    pub fn references_publication(&self, publication: u64) -> bool {
        self.open_segment
            .as_ref()
            .is_some_and(|open| open.publication == publication)
            || self
                .publication_refs
                .get(&publication)
                .is_some_and(|count| *count > 0)
    }

    /// Capture durable identifiers under the write lock, before a takeover can
    /// replace the active rendition. This copies metadata, never media bytes.
    pub fn ready_segment(
        &self,
        stream: &crate::domain::StreamId,
    ) -> Option<crate::observe::lifecycle::ReadySegment> {
        use crate::delivery::uri::{MediaResource, PercentEncoded, append_media_leaf};
        let id = self.visible_segments.back()?;
        let segment = &self.segment_resources.get(id)?.segment;
        let StoredSegmentKind::Media(body) = &segment.kind else {
            return None;
        };
        let format = self.descriptor.config.segment_format;
        let path = |resource| {
            let mut leaf = String::new();
            append_media_leaf(&mut leaf, resource)?;
            Some(format!(
                "/{}/{}/{}",
                PercentEncoded(&stream.0),
                self.rendition_id.0,
                leaf
            ))
        };
        Some(crate::observe::lifecycle::ReadySegment {
            rendition_id: self.rendition_id.0,
            segment_id: segment.id.0,
            media_sequence: segment.msn.0,
            publication: segment.publication,
            path: path(MediaResource::Segment(
                self.rendition_id,
                segment.id,
                format,
            ))?,
            initialization_path: path(MediaResource::Initialization(
                self.rendition_id,
                segment.initialization,
                format,
            )),
            media_start: segment.media_start,
            duration: segment.duration,
            timebase: segment.timebase,
            bytes: body.len(),
            independent: segment.independent,
            discontinuity: segment.discontinuity_before,
        })
    }

    fn snapshot(&self) -> RenditionSnapshot {
        let visible = |part: &Arc<StoredPart>| {
            self.part_resources
                .get(&part.id)
                .is_some_and(|resource| resource.playlist_visible)
        };
        let parts_visible_from = self
            .part_order
            .front()
            .and_then(|id| self.part_resources.get(id))
            .map(|resource| resource.part.cursor.msn);
        let open_segment = self.open_segment.as_ref().map(|open| {
            let mut open = open.clone();
            open.parts.retain(&visible);
            open
        });
        RenditionSnapshot {
            rendition_id: self.rendition_id,
            media_kind: self.descriptor.media.kind(),
            config: self.advertised_config,
            contract: self.contract,
            media_sequence: self.media_sequence,
            discontinuity_sequence: self.discontinuity_sequence,
            initializations: Arc::clone(&self.initializations),
            segments: PublishedSegments::new(
                Arc::clone(&self.published_segments),
                parts_visible_from,
            ),
            open_segment,
            live_edge: self.live_edge,
            bitrate: self.bitrate.snapshot(),
        }
    }

    /// Republishes the request-facing snapshot without advancing the edge.
    ///
    /// Used by retention sweeps, which change what is retained but do not add
    /// media for a blocked reader to wake up for.
    pub fn publish_snapshot(&self) {
        self.published.publish(self.snapshot());
    }

    /// Advances the live edge and republishes the snapshot behind it.
    ///
    /// These belong together: a snapshot published without a matching edge
    /// revision leaves blocked readers waiting on media they could already
    /// fetch, and an edge announced without its snapshot points at media the
    /// request path cannot yet see. The returned update is handed to
    /// [`notify_edges`] once the state lock is released.
    #[must_use = "a committed edge must be announced by notify_edges"]
    pub fn commit(&mut self, ended: bool) -> EdgeUpdate {
        let edge = self.advance_edge(ended);
        self.publish_snapshot();
        (self.edge_updates.clone(), edge)
    }

    /// A retired playlist receives no new media to drive ordinary window trim.
    /// Keep its final window plus fetch grace, then remove the entire playlist.
    pub fn retire(&mut self, now: Instant, retention: RetentionPolicy) {
        if self.retired {
            return; // Later reconnects must not renew an old playlist's lifetime.
        }
        self.retired = true;
        let target = self.contract.target_duration();
        let window = retention
            .minimum_playlist_duration_for(target)
            .max(self.visible_playlist_duration());
        let grace = retention
            .part_fetch_grace_period
            .resolve(target)
            .max(target);
        self.retirement_deadline = now.checked_add(window.saturating_add(grace));
    }

    pub fn retirement_expired(&self, now: Instant) -> bool {
        self.retired
            && (self
                .retirement_deadline
                .is_some_and(|deadline| now >= deadline)
                || (self.retained_object_counts() == (0, 0, 0) && self.open_segment.is_none()))
    }

    pub fn retained_object_counts(&self) -> (usize, usize, usize) {
        (
            self.part_resources.len(),
            self.segment_resources.len(),
            self.initializations.len(),
        )
    }

    pub fn additional_bytes_for(
        &self,
        media: &PackagedMedia,
        gzip: Option<&Payload>,
    ) -> Result<usize, StoreWriteError> {
        self.validate(media)?;
        Ok(match media {
            PackagedMedia::Initialization(segment) => {
                if self.holds_current_initialization(segment) {
                    0
                } else {
                    memory_len(segment.payload.len(), gzip)
                }
            }
            PackagedMedia::Chunk(chunk) => memory_len(chunk.payload.len(), gzip),
            PackagedMedia::Segment(segment) => memory_len(segment.payload.len(), gzip),
            PackagedMedia::SegmentCompleted(_) | PackagedMedia::Gap(_) => 0,
        })
    }

    /// Whether the current initialization already *is* this header.
    ///
    /// Compares rendition and version before payload bytes, so a muxer that
    /// repeats an identical header costs neither a comparison of megabytes nor
    /// a second durable ID.
    fn holds_current_initialization(&self, segment: &InitializationSegment) -> bool {
        self.current_initialization.is_some_and(|current| {
            self.initializations.iter().any(|held| {
                held.id == current
                    && held.version == segment.version
                    && held.payload == segment.payload
            })
        })
    }

    fn validate(&self, media: &PackagedMedia) -> Result<(), StoreWriteError> {
        let rendition_id = self.rendition_id;
        match media {
            PackagedMedia::Initialization(_) => {
                self.require_config()?;
                if self.open_segment.is_some() {
                    return Err(StoreWriteError::InitializationDuringOpenSegment { rendition_id });
                }
            }
            PackagedMedia::Chunk(chunk) => self.validate_chunk(chunk)?,
            PackagedMedia::Segment(segment) => {
                let config = self.require_media_ready()?;
                if self.open_segment.is_some() {
                    return Err(StoreWriteError::DirectSegmentDuringOpenSegment { rendition_id });
                }
                if config.chunk_target.is_some() {
                    return Err(StoreWriteError::DirectSegmentsDisabled { rendition_id });
                }
                self.require_next_packaging_segment_id(segment.packaging_segment_id)?;
                self.require_permitted_segment(
                    config.timebase.ticks_to_duration(segment.duration),
                )?;
            }
            PackagedMedia::Gap(gap) => {
                let config = self.require_media_ready()?;
                if self.open_segment.is_some() {
                    return Err(StoreWriteError::DirectSegmentDuringOpenSegment { rendition_id });
                }
                self.require_next_packaging_segment_id(gap.packaging_segment_id)?;
                self.require_permitted_segment(config.timebase.ticks_to_duration(gap.duration))?;
                let total = gap.parts.iter().try_fold(0_u64, |sum, duration| {
                    if *duration == 0
                        || config
                            .chunk_target
                            .is_some_and(|target| *duration > target.get())
                    {
                        None
                    } else {
                        sum.checked_add(*duration)
                    }
                });
                if gap.duration == 0
                    || gap.media_start.checked_add_unsigned(gap.duration).is_none()
                    || (config.chunk_target.is_some() && total != Some(gap.duration))
                    || (config.chunk_target.is_none() && !gap.parts.is_empty())
                {
                    return Err(StoreWriteError::SegmentTimingMismatch { rendition_id });
                }
            }
            PackagedMedia::SegmentCompleted(completion) => {
                let config = self.require_media_ready()?;
                if config.chunk_target.is_none() {
                    return Err(StoreWriteError::ChunksDisabled { rendition_id });
                }
                let Some(open) = &self.open_segment else {
                    return Err(StoreWriteError::NoOpenSegment { rendition_id });
                };
                if open.packaging_segment_id != completion.packaging_segment_id {
                    return Err(StoreWriteError::WrongSegmentCompleted {
                        rendition_id,
                        open: open.packaging_segment_id,
                        found: completion.packaging_segment_id,
                    });
                }
                if open.media_start != completion.media_start
                    || open.duration != completion.duration
                {
                    return Err(StoreWriteError::SegmentTimingMismatch { rendition_id });
                }
                self.require_permitted_segment(
                    config.timebase.ticks_to_duration(completion.duration),
                )?;
            }
        }
        Ok(())
    }

    fn validate_chunk(&self, chunk: &PackagedChunk) -> Result<(), StoreWriteError> {
        let rendition_id = self.rendition_id;
        let config = self.require_media_ready()?;
        if config.chunk_target.is_none() {
            return Err(StoreWriteError::ChunksDisabled { rendition_id });
        }
        self.require_permitted_part(config.timebase.ticks_to_duration(chunk.duration))?;
        match &self.open_segment {
            Some(open) if open.packaging_segment_id != chunk.packaging_segment_id => {
                Err(StoreWriteError::DifferentSegmentAlreadyOpen {
                    rendition_id,
                    open: open.packaging_segment_id,
                    found: chunk.packaging_segment_id,
                })
            }
            Some(open) => {
                let expected = u32::try_from(open.parts.len()).unwrap_or(u32::MAX);
                if chunk.chunk_index != expected {
                    return Err(StoreWriteError::UnexpectedChunkIndex {
                        rendition_id,
                        expected,
                        found: chunk.chunk_index,
                    });
                }
                if open.media_start.checked_add_unsigned(open.duration) != Some(chunk.media_start) {
                    return Err(StoreWriteError::SegmentTimingMismatch { rendition_id });
                }
                // A part's 85% floor only applies once something follows it, so
                // the arrival of a successor is the first moment the
                // predecessor can be judged — and the last moment before it
                // becomes unfixable.
                if let Some(previous) = open.parts.last()
                    && !previous.independent
                {
                    self.require_permitted_non_final_part(
                        config.timebase.ticks_to_duration(previous.duration),
                    )?;
                }
                self.require_permitted_segment(
                    config
                        .timebase
                        .ticks_to_duration(open.duration.saturating_add(chunk.duration)),
                )
            }
            None => {
                self.require_next_packaging_segment_id(chunk.packaging_segment_id)?;
                if chunk.chunk_index != 0 {
                    return Err(StoreWriteError::UnexpectedChunkIndex {
                        rendition_id,
                        expected: 0,
                        found: chunk.chunk_index,
                    });
                }
                self.require_permitted_segment(config.timebase.ticks_to_duration(chunk.duration))
            }
        }
    }

    fn require_config(&self) -> Result<RenditionConfig, StoreWriteError> {
        self.active_config
            .map(|(_, config)| config)
            .ok_or(StoreWriteError::RenditionInactive {
                rendition_id: self.rendition_id,
            })
    }

    fn require_media_ready(&self) -> Result<RenditionConfig, StoreWriteError> {
        let config = self.require_config()?;
        if self.current_initialization.is_none() {
            return Err(StoreWriteError::InitializationMissing {
                rendition_id: self.rendition_id,
            });
        }
        Ok(config)
    }

    /// Rejects media the advertised target duration could not cover.
    ///
    /// Checked for an accumulating segment on every chunk rather than only at
    /// completion: an over-long segment's parts are fetchable and tagged long
    /// before it closes, so refusing it at completion would refuse media that
    /// viewers had already been offered.
    fn require_permitted_segment(&self, duration: Duration) -> Result<(), StoreWriteError> {
        if self.contract.permits_segment(duration) {
            return Ok(());
        }
        Err(StoreWriteError::SegmentTooLong {
            rendition_id: self.rendition_id,
            duration,
            maximum: self.contract.maximum_segment_duration,
        })
    }

    fn require_permitted_part(&self, duration: Duration) -> Result<(), StoreWriteError> {
        if self.contract.permits_part(duration) {
            return Ok(());
        }
        Err(StoreWriteError::PartTooLong {
            rendition_id: self.rendition_id,
            duration,
            maximum: self.contract.part_target.unwrap_or(Duration::ZERO),
        })
    }

    fn require_permitted_non_final_part(&self, duration: Duration) -> Result<(), StoreWriteError> {
        if self.contract.permits_non_final_part(duration) {
            return Ok(());
        }
        Err(StoreWriteError::PartTooShort {
            rendition_id: self.rendition_id,
            duration,
            minimum: self
                .contract
                .minimum_non_final_part_duration()
                .unwrap_or(Duration::ZERO),
        })
    }

    fn require_next_packaging_segment_id(
        &self,
        found: PackagingSegmentId,
    ) -> Result<(), StoreWriteError> {
        if let Some(previous) = self.last_packaging_segment_id
            && found <= previous
        {
            return Err(StoreWriteError::NonMonotonicSegmentId {
                rendition_id: self.rendition_id,
                previous,
                found,
            });
        }
        Ok(())
    }

    /// `gzip` is the encoding delivery will serve this media under, already
    /// computed by the publisher. Stored beside the payload and never inspected
    /// here; a completed chunked segment gets none, because it is served from
    /// the parts that composed it and was never a single buffer to encode.
    pub fn apply(
        &mut self,
        publication: u64,
        media: PackagedMedia,
        gzip: Option<Payload>,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        match media {
            PackagedMedia::Initialization(segment) => self.set_initialization(segment, gzip),
            PackagedMedia::Chunk(chunk) => {
                self.push_chunk(publication, chunk, gzip, now, retention);
            }
            PackagedMedia::Segment(segment) => {
                self.push_direct_segment(publication, segment, gzip, now, retention);
            }
            PackagedMedia::SegmentCompleted(completion) => {
                self.complete_segment(completion, now, retention);
            }
            PackagedMedia::Gap(gap) => self.push_gap(publication, &gap, now, retention),
        }
    }

    fn push_gap(
        &mut self,
        publication: u64,
        gap: &crate::mux::PackagedGap,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        let config = self.require_config().expect("validated configuration");
        // Reuse part identity/retention bookkeeping, never the abandoned-parent
        // operation: that operation replaces media and substitutes a target duration.
        let mut start = gap.media_start;
        for (index, duration) in gap.parts.iter().copied().enumerate() {
            self.push_chunk(
                publication,
                PackagedChunk {
                    rendition_id: gap.rendition_id,
                    packaging_segment_id: gap.packaging_segment_id,
                    chunk_index: u32::try_from(index).expect("bounded gap part count"),
                    media_start: start,
                    duration,
                    independent: false,
                    payload: Payload::default(),
                },
                None,
                now,
                retention,
            );
            let open = self.open_segment.as_mut().expect("gap opened parent");
            let part = open.parts.last_mut().expect("gap appended part");
            Arc::make_mut(part).gap = true;
            self.part_resources
                .get_mut(&part.id)
                .expect("gap resource exists")
                .part = Arc::clone(part);
            start = start
                .checked_add_unsigned(duration)
                .expect("validated gap span");
        }
        let (id, msn, discontinuity_before, parts) = if let Some(open) = self.open_segment.take() {
            (open.id, open.msn, open.discontinuity_before, open.parts)
        } else {
            self.issued_segments = self.issued_segments.saturating_add(1);
            (
                SegmentId(self.issued_segments),
                Msn(self.next_msn),
                self.opens_discontinuity(publication),
                Vec::new(),
            )
        };
        for part in &parts {
            self.part_resources
                .get_mut(&part.id)
                .expect("gap resource exists")
                .parent_retained = true;
        }
        self.last_parent_publication = Some(publication);
        self.gaps += 1;
        self.publication_totals.0.lock().gaps += 1;
        self.bitrate.break_contiguity();
        let segment = StoredSegment {
            iframe_msn: 0,
            iframes: [].into(),
            id,
            msn,
            publication,
            initialization: self
                .current_initialization
                .expect("validated initialization"),
            media_start: gap.media_start,
            duration: gap.duration,
            timebase: config.timebase,
            independent: false,
            discontinuity_before,
            kind: StoredSegmentKind::GapParts(parts.into()),
            gzip: None,
        };
        self.advance_playlist(
            gap.packaging_segment_id,
            config.timebase.ticks_to_duration(gap.duration),
        );
        self.insert_segment(segment, now, retention);
    }

    fn set_initialization(&mut self, segment: InitializationSegment, gzip: Option<Payload>) {
        // Initialization updates are rare, and repeating one is not an error:
        // a reconnecting publisher commonly re-sends the header it already
        // sent, and issuing a second ID for identical bytes would keep media
        // pointing at a section nothing distinguishes from the current one.
        if self.holds_current_initialization(&segment) {
            return;
        }
        let payload_bytes = memory_len(segment.payload.len(), gzip.as_ref());
        self.issued_initializations = self.issued_initializations.saturating_add(1);
        let id = InitializationId(self.issued_initializations);
        self.current_initialization = Some(id);
        let mut initializations = self.initializations.to_vec();
        initializations.push(StoredInitialization {
            id,
            version: segment.version,
            payload: segment.payload,
            gzip,
        });
        self.initializations = initializations.into();
        self.retained_payload_bytes = self.retained_payload_bytes.saturating_add(payload_bytes);
    }

    fn push_chunk(
        &mut self,
        publication: u64,
        chunk: PackagedChunk,
        gzip: Option<Payload>,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        let config = self.require_config().expect("validated configuration");
        let payload_bytes = memory_len(chunk.payload.len(), gzip.as_ref());
        let initialization = self
            .current_initialization
            .expect("validated initialization");
        if self.open_segment.is_none() {
            self.issued_segments = self.issued_segments.saturating_add(1);
            self.open_segment = Some(OpenSegment {
                id: SegmentId(self.issued_segments),
                msn: Msn(self.next_msn),
                publication,
                initialization,
                packaging_segment_id: chunk.packaging_segment_id,
                media_start: chunk.media_start,
                duration: 0,
                discontinuity_before: self.opens_discontinuity(publication),
                parts: Vec::new(),
            });
            self.last_parent_publication = Some(publication);
            self.refresh_media_sequence();
        }

        self.issued_parts = self.issued_parts.saturating_add(1);
        let open = self.open_segment.as_mut().expect("open segment exists");
        let cursor = PartCursor {
            msn: open.msn,
            part_index: PartIndex(chunk.chunk_index),
        };
        let id = PartId(self.issued_parts);
        let part = Arc::new(StoredPart {
            gap: false,
            id,
            cursor,
            publication,
            initialization,
            media_start: chunk.media_start,
            duration: chunk.duration,
            timebase: config.timebase,
            independent: chunk.independent,
            payload: chunk.payload.into(),
            gzip: gzip.map(Into::into),
        });
        open.duration = open.duration.saturating_add(chunk.duration);
        open.parts.push(Arc::clone(&part));

        let playlist_end = self
            .playlist_position
            .saturating_add(config.timebase.ticks_to_duration(open.duration));
        self.part_order.push_back(id);
        self.part_resources.insert(
            id,
            PartResource {
                part,
                playlist_end,
                segment_target: config
                    .timebase
                    .ticks_to_duration(config.segment_target.get()),
                playlist_visible: true,
                expires_at: None,
                parent_retained: false,
            },
        );
        self.retain_publication(publication);
        self.retained_payload_bytes = self.retained_payload_bytes.saturating_add(payload_bytes);
        self.hide_old_parts(now, retention);
    }

    fn complete_segment(
        &mut self,
        completion: PackagedSegmentCompletion,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        let config = self.require_config().expect("validated configuration");
        let open = self.open_segment.take().expect("validated open segment");
        let parts: Arc<[Arc<StoredPart>]> = open.parts.into();
        for part in parts.iter() {
            if let Some(resource) = self.part_resources.get_mut(&part.id) {
                resource.parent_retained = true;
            }
        }
        let segment = StoredSegment {
            iframe_msn: 0,
            iframes: super::iframe::ranges(
                self.descriptor.media.kind(),
                config.segment_format,
                parts.iter().map(|part| {
                    (
                        part.payload.as_bytes(),
                        part.media_start.saturating_sub(completion.media_start),
                    )
                }),
                completion.duration,
            ),
            id: open.id,
            msn: open.msn,
            publication: open.publication,
            initialization: open.initialization,
            media_start: completion.media_start,
            duration: completion.duration,
            timebase: config.timebase,
            independent: parts.first().is_some_and(|part| part.independent),
            // Decided when the segment opened; completing it only preserves
            // the answer its already-published parts were tagged under.
            discontinuity_before: open.discontinuity_before,
            kind: StoredSegmentKind::Media(SegmentBody::Chunked(Arc::clone(&parts))),
            // Served from the parts that composed it, so there is no single
            // buffer anyone encoded.
            gzip: None,
        };
        self.commit_segment(
            segment,
            completion.packaging_segment_id,
            config,
            now,
            retention,
        );
    }

    fn push_direct_segment(
        &mut self,
        publication: u64,
        packaged: PackagedSegment,
        gzip: Option<Payload>,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        let config = self.require_config().expect("validated configuration");
        let initialization = self
            .current_initialization
            .expect("validated initialization");
        self.issued_segments = self.issued_segments.saturating_add(1);
        let segment = StoredSegment {
            iframe_msn: 0,
            iframes: super::iframe::ranges(
                self.descriptor.media.kind(),
                config.segment_format,
                std::iter::once((packaged.payload.as_bytes(), 0)),
                packaged.duration,
            ),
            id: SegmentId(self.issued_segments),
            msn: Msn(self.next_msn),
            publication,
            initialization,
            media_start: packaged.media_start,
            duration: packaged.duration,
            timebase: config.timebase,
            independent: packaged.independent,
            discontinuity_before: self.opens_discontinuity(publication),
            kind: StoredSegmentKind::Media(SegmentBody::Contiguous(packaged.payload.into())),
            gzip: gzip.map(Into::into),
        };
        self.last_parent_publication = Some(publication);
        self.commit_segment(
            segment,
            packaged.packaging_segment_id,
            config,
            now,
            retention,
        );
    }

    /// Records one segment the publisher actually produced.
    ///
    /// Shared by both packaging modes: a chunked segment closed by its
    /// completion and a direct segment differ only in how their bytes were
    /// assembled, and everything downstream of that — numbering, the playlist
    /// clock, bitrate, retention — is the same work.
    fn commit_segment(
        &mut self,
        segment: StoredSegment,
        packaging_segment_id: PackagingSegmentId,
        config: RenditionConfig,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        let duration = config.timebase.ticks_to_duration(segment.duration);
        let bytes = segment_byte_len(&segment);
        self.advance_playlist(packaging_segment_id, duration);
        self.bitrate.observe(
            bytes,
            duration,
            config
                .timebase
                .ticks_to_duration(config.segment_target.get()),
        );
        self.insert_segment(segment, now, retention);
    }

    /// Advances durable numbering and the playlist clock past one closed
    /// segment.
    ///
    /// Kept apart from [`Self::commit_segment`] because a synthesized gap
    /// advances both but is not a bitrate observation: it carries no bytes any
    /// publisher produced, and counting it would drag the advertised average
    /// toward zero every time a session was interrupted.
    fn advance_playlist(&mut self, packaging_segment_id: PackagingSegmentId, duration: Duration) {
        self.last_packaging_segment_id = Some(packaging_segment_id);
        self.next_msn = self.next_msn.saturating_add(1);
        self.playlist_position = self.playlist_position.saturating_add(duration);
    }

    fn insert_segment(
        &mut self,
        mut segment: StoredSegment,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        segment.iframe_msn = self.next_iframe_msn;
        self.next_iframe_msn = self.next_iframe_msn.saturating_add(segment.iframe_count());
        let id = segment.id;
        let publication = segment.publication;
        let payload_bytes = segment_resource_bytes(&segment);
        self.visible_segments.push_back(id);
        self.segment_resources.insert(
            id,
            SegmentResource {
                segment: Arc::new(segment),
                visible: true,
                first_published_at: now,
                longest_playlist_duration: Duration::ZERO,
                expires_at: None,
            },
        );
        self.retained_payload_bytes = self.retained_payload_bytes.saturating_add(payload_bytes);
        self.retain_publication(publication);

        let segment_target = self.advertised_config.map_or(Duration::MAX, |config| {
            config
                .timebase
                .ticks_to_duration(config.segment_target.get())
        });
        let minimum_playlist_duration = retention.minimum_playlist_duration_for(segment_target);
        let mut playlist_duration = self.visible_playlist_duration();
        while self.visible_segments.len() > MINIMUM_PLAYLIST_SEGMENTS {
            let Some(removed) = self.visible_segments.front().copied() else {
                break;
            };
            let Some(resource) = self.segment_resources.get(&removed) else {
                self.visible_segments.pop_front();
                continue;
            };
            let removed_duration = resource
                .segment
                .timebase
                .ticks_to_duration(resource.segment.duration);
            if playlist_duration.saturating_sub(removed_duration) < minimum_playlist_duration {
                break;
            }

            self.visible_segments.pop_front();
            playlist_duration = playlist_duration.saturating_sub(removed_duration);
            let resource = self
                .segment_resources
                .get_mut(&removed)
                .expect("visible segment resource exists");
            // A tag leaving the window is exactly what
            // EXT-X-DISCONTINUITY-SEQUENCE counts, so the increment belongs
            // here and nowhere else: the tag itself stays printed for as long
            // as its segment is visible.
            if resource.segment.discontinuity_before {
                self.discontinuity_sequence = self.discontinuity_sequence.saturating_add(1);
            }
            resource.visible = false;
            resource.expires_at =
                retention.segment_fetch_deadline(resource.first_published_at, now, segment_target);
            self.retired_segments.push_back(removed);
        }
        for id in &self.visible_segments {
            if let Some(resource) = self.segment_resources.get_mut(id) {
                resource.longest_playlist_duration =
                    resource.longest_playlist_duration.max(playlist_duration);
            }
        }
        self.refresh_media_sequence();
        self.hide_old_parts(now, retention);
        self.sweep_expired(now);
        self.refresh_published_segments();
    }

    /// Whether a parent segment created for `publication` follows a splice.
    ///
    /// The first parent a rendition ever creates does not: nothing precedes it
    /// to be discontinuous with.
    fn opens_discontinuity(&self, publication: u64) -> bool {
        self.last_parent_publication
            .is_some_and(|previous| previous != publication)
    }

    /// Recomputes the MSN a media playlist must advertise.
    ///
    /// The window head when there is one. Otherwise the open segment, whose
    /// MSN is already observable through its parts, and failing that the MSN
    /// the next segment will take — so a playlist with nothing to show still
    /// names where it is rather than resetting to zero.
    fn refresh_media_sequence(&mut self) {
        self.media_sequence = self
            .visible_segments
            .front()
            .and_then(|id| self.segment_resources.get(id))
            .map(|resource| resource.segment.msn.0)
            .or_else(|| self.open_segment.as_ref().map(|open| open.msn.0))
            .unwrap_or(self.next_msn);
    }

    /// Resolves an open segment nobody will complete into a playlist gap.
    ///
    /// The MSN was already observable, so it cannot simply vanish: a reader
    /// that saw it would wait forever for media that is never coming.
    pub fn finish_open_as_gap(&mut self, now: Instant, retention: RetentionPolicy) {
        let Some(open) = self.open_segment.take() else {
            return;
        };
        let Some(config) = self.advertised_config else {
            return;
        };
        self.gaps += 1;
        self.publication_totals.0.lock().gaps += 1;
        self.bitrate.break_contiguity();
        let msn = open.msn;
        self.unpick_visible_parts_from_back(msn);
        for part in &open.parts {
            if let Some(resource) = self.part_resources.get_mut(&part.id) {
                resource.playlist_visible = false;
                resource.parent_retained = false;
                resource.expires_at = retention.part_fetch_deadline(now, resource.segment_target);
            }
            self.orphaned_parts.push_back(part.id);
        }
        let duration = config.segment_target.get();
        let segment = StoredSegment {
            iframe_msn: 0,
            iframes: [].into(),
            id: open.id,
            msn: open.msn,
            publication: open.publication,
            initialization: open.initialization,
            media_start: open.media_start,
            duration,
            timebase: config.timebase,
            independent: false,
            discontinuity_before: open.discontinuity_before,
            kind: StoredSegmentKind::Gap,
            // A gap has no bytes, so there is nothing to encode.
            gzip: None,
        };
        self.advance_playlist(
            open.packaging_segment_id,
            config.timebase.ticks_to_duration(duration),
        );
        self.insert_segment(segment, now, retention);
    }

    fn hide_old_parts(&mut self, now: Instant, retention: RetentionPolicy) {
        let live_position = self.current_live_position();
        let open_msn = self.open_segment.as_ref().map(|open| open.msn);

        // `part_order` is only playlist-visible tags. Age whole completed
        // parents from the front; stop at the open segment or the first parent
        // still inside the part-tag window. Already-hidden tags are not here.
        while let Some(front_id) = self.part_order.front().copied() {
            let Some(front) = self.part_resources.get(&front_id) else {
                self.part_order.pop_front();
                continue;
            };
            if open_msn == Some(front.part.cursor.msn) {
                break;
            }
            let msn = front.part.cursor.msn;
            let mut last_part = front_id;
            for id in &self.part_order {
                let Some(resource) = self.part_resources.get(id) else {
                    continue;
                };
                if resource.part.cursor.msn != msn {
                    break;
                }
                last_part = *id;
            }
            let last = self
                .part_resources
                .get(&last_part)
                .expect("the prefix part was just read");
            let maximum_age = retention.part_tag_retention_for(last.segment_target);
            if live_position.saturating_sub(last.playlist_end) <= maximum_age {
                break;
            }
            while self.part_order.front().is_some_and(|id| {
                self.part_resources
                    .get(id)
                    .is_some_and(|resource| resource.part.cursor.msn == msn)
            }) {
                let id = self.part_order.pop_front().expect("front exists");
                if let Some(resource) = self.part_resources.get_mut(&id) {
                    resource.playlist_visible = false;
                    resource.expires_at =
                        retention.part_fetch_deadline(now, resource.segment_target);
                }
            }
        }
    }

    /// When the oldest capacity-evictable segment was published, if there is one.
    ///
    /// Retired (already hidden) parents first, then the oldest visible parent
    /// only when hiding it would still leave a legal live playlist.
    pub fn oldest_shed_candidate(&self) -> Option<Instant> {
        if let Some(id) = self.retired_segments.front() {
            if self.spilling.contains_key(id) {
                return None;
            }
            return self
                .segment_resources
                .get(id)
                .map(|resource| resource.first_published_at);
        }
        if self.can_hide_oldest_visible() {
            if self
                .visible_segments
                .front()
                .is_some_and(|id| self.spilling.contains_key(id))
            {
                return None;
            }
            return self
                .visible_segments
                .front()
                .and_then(|id| self.segment_resources.get(id))
                .map(|resource| resource.first_published_at);
        }
        None
    }

    fn can_hide_oldest_visible(&self) -> bool {
        if self.visible_segments.len() <= MINIMUM_PLAYLIST_SEGMENTS {
            return false;
        }
        let Some(front) = self.visible_segments.front() else {
            return false;
        };
        let Some(resource) = self.segment_resources.get(front) else {
            return false;
        };
        let removed = resource
            .segment
            .timebase
            .ticks_to_duration(resource.segment.duration);
        let remaining = self.visible_playlist_duration().saturating_sub(removed);
        remaining >= self.protocol_playlist_floor()
    }

    /// Three target durations, the HLS live-playlist floor.
    ///
    /// Distinct from [`RetentionPolicy::minimum_playlist_duration_for`], which
    /// is `retain` itself when that is longer. Capacity eviction must be able
    /// to shrink *below* `retain`; that method is the time-based trim.
    fn protocol_playlist_floor(&self) -> Duration {
        let target = self.advertised_config.map_or(Duration::MAX, |config| {
            config
                .timebase
                .ticks_to_duration(config.segment_target.get())
        });
        target.saturating_mul(u32::try_from(MINIMUM_PLAYLIST_SEGMENTS).unwrap_or(3))
    }

    /// Drops the oldest capacity victim: a retired parent, or a visible one
    /// above the playlist floor.
    ///
    /// Capacity is not `retain` trim. The parent and its parts leave immediately,
    /// so the byte budget actually falls. Returns whether bytes or object counts
    /// decreased; a hide that frees nothing is not progress for the shed loop.
    pub fn shed_oldest(&mut self) -> bool {
        let before_bytes = self.retained_payload_bytes;
        let before_disk = self.retained_disk_bytes;
        let before_parts = self.part_resources.len();
        let before_segments = self.segment_resources.len();

        let id = if let Some(id) = self.retired_segments.pop_front() {
            id
        } else if self.can_hide_oldest_visible() {
            self.hide_oldest_visible_for_capacity()
        } else {
            return false;
        };
        self.release_segment(id, PartFate::Immediate);
        self.forget_unreachable_initializations();
        self.refresh_published_segments();

        self.retained_payload_bytes < before_bytes
            || self.retained_disk_bytes < before_disk
            || self.part_resources.len() < before_parts
            || self.segment_resources.len() < before_segments
    }

    fn hide_oldest_visible_for_capacity(&mut self) -> SegmentId {
        let id = self
            .visible_segments
            .pop_front()
            .expect("can_hide_oldest_visible requires a visible parent");
        if let Some(resource) = self.segment_resources.get_mut(&id) {
            if resource.segment.discontinuity_before {
                self.discontinuity_sequence = self.discontinuity_sequence.saturating_add(1);
            }
            resource.visible = false;
            resource.expires_at = Some(Instant::now());
        }
        self.refresh_media_sequence();
        id
    }

    pub fn sweep_expired(&mut self, now: Instant) {
        self.compact_spilled_segments(now);
        while let Some(id) = self.retired_segments.front().copied() {
            let Some(resource) = self.segment_resources.get(&id) else {
                self.retired_segments.pop_front();
                continue;
            };
            if resource
                .expires_at
                .is_none_or(|expires_at| now < expires_at)
            {
                break;
            }
            self.retired_segments.pop_front();
            self.release_segment(id, PartFate::Grace);
        }

        while let Some(id) = self.orphaned_parts.front().copied() {
            let Some(resource) = self.part_resources.get(&id) else {
                self.orphaned_parts.pop_front();
                continue;
            };
            if resource
                .expires_at
                .is_none_or(|expires_at| now < expires_at)
            {
                break;
            }
            self.orphaned_parts.pop_front();
            self.drop_part(id);
        }
    }

    fn refresh_published_segments(&mut self) {
        self.published_segments = self
            .visible_segments
            .iter()
            .filter_map(|id| self.segment_resources.get(id))
            .map(|resource| Arc::clone(&resource.segment))
            .collect::<Vec<_>>()
            .into();
    }

    fn retain_publication(&mut self, publication: u64) {
        self.publication_refs
            .entry(publication)
            .and_modify(|count| *count = count.saturating_add(1))
            .or_insert(1);
    }

    fn release_publication(&mut self, publication: u64) {
        let Some(count) = self.publication_refs.get_mut(&publication) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.publication_refs.remove(&publication);
        }
    }

    fn unpick_visible_parts(&mut self, msn: Msn) {
        while self.part_order.front().is_some_and(|id| {
            self.part_resources
                .get(id)
                .is_some_and(|resource| resource.part.cursor.msn == msn)
        }) {
            self.part_order.pop_front();
        }
    }

    fn unpick_visible_parts_from_back(&mut self, msn: Msn) {
        while self.part_order.back().is_some_and(|id| {
            self.part_resources
                .get(id)
                .is_some_and(|resource| resource.part.cursor.msn == msn)
        }) {
            self.part_order.pop_back();
        }
    }

    fn release_segment(&mut self, id: SegmentId, parts: PartFate) {
        let Some(resource) = self.segment_resources.remove(&id) else {
            return;
        };
        self.retained_payload_bytes = self
            .retained_payload_bytes
            .saturating_sub(segment_resource_bytes(&resource.segment));
        self.retained_disk_bytes = self
            .retained_disk_bytes
            .saturating_sub(segment_disk_bytes(&resource.segment));
        unlink_segment_files(&resource.segment);
        self.spilling.remove(&id);
        self.release_publication(resource.segment.publication);
        self.unpick_visible_parts(resource.segment.msn);
        if let StoredSegmentKind::Media(SegmentBody::Chunked(chunk_parts))
        | StoredSegmentKind::GapParts(chunk_parts) = &resource.segment.kind
        {
            for part in chunk_parts.iter() {
                match parts {
                    PartFate::Immediate => self.drop_part(part.id),
                    PartFate::Grace => self.orphan_part(part.id),
                }
            }
        }
    }

    fn orphan_part(&mut self, id: PartId) {
        {
            let Some(resource) = self.part_resources.get_mut(&id) else {
                return;
            };
            resource.parent_retained = false;
            resource.playlist_visible = false;
            if resource.expires_at.is_none() {
                resource.expires_at = Some(Instant::now());
            }
        }
        self.orphaned_parts.push_back(id);
    }

    fn drop_part(&mut self, id: PartId) {
        let Some(resource) = self.part_resources.remove(&id) else {
            return;
        };
        self.retained_payload_bytes = self.retained_payload_bytes.saturating_sub(
            resource.part.payload.memory_bytes().saturating_add(
                resource
                    .part
                    .gzip
                    .as_ref()
                    .map_or(0, HeldBytes::memory_bytes),
            ),
        );
        self.retained_disk_bytes = self.retained_disk_bytes.saturating_sub(
            resource
                .part
                .payload
                .disk_bytes()
                .saturating_add(resource.part.gzip.as_ref().map_or(0, HeldBytes::disk_bytes)),
        );
        self.unlink_unreferenced(&resource.part.payload);
        if let Some(gzip) = &resource.part.gzip {
            self.unlink_unreferenced(gzip);
        }
        self.release_publication(resource.part.publication);
    }

    /// Parts share their parent's file, including while orphaned for fetch grace.
    fn unlink_unreferenced(&self, held: &HeldBytes) {
        let HeldBytes::Disk(disk) = held else {
            return;
        };
        let same =
            |held: &HeldBytes| matches!(held, HeldBytes::Disk(other) if other.path == disk.path);
        let retained = self.part_resources.values().any(|resource| {
            same(&resource.part.payload) || resource.part.gzip.as_ref().is_some_and(same)
        }) || self.segment_resources.values().any(|resource| {
            matches!(&resource.segment.kind, StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) if same(payload))
                || resource.segment.gzip.as_ref().is_some_and(same)
        });
        if !retained {
            held.unlink_disk();
        }
    }

    /// Once every part URI has expired, one locator replaces the part graph.
    fn compact_spilled_segments(&mut self, now: Instant) {
        let candidates: Vec<_> = self
            .chunked_spills
            .iter()
            .filter_map(|id| {
                let resource = self.segment_resources.get(id)?;
                let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &resource.segment.kind
                else {
                    return None;
                };
                let first = parts.first()?;
                let HeldBytes::Disk(first_disk) = &first.payload else {
                    return None;
                };
                let eligible = parts.iter().all(|part| {
                    matches!(&part.payload, HeldBytes::Disk(disk) if disk.path == first_disk.path)
                        && self.part_resources.get(&part.id).is_some_and(|resource| {
                            !resource.playlist_visible
                                && resource.expires_at.is_some_and(|deadline| now >= deadline)
                        })
                });
                eligible.then_some(*id)
            })
            .collect();
        let changed = !candidates.is_empty();
        for id in candidates {
            let resource = self
                .segment_resources
                .get_mut(&id)
                .expect("candidate exists");
            let mut segment = (*resource.segment).clone();
            let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &segment.kind else {
                continue;
            };
            let parts = Arc::clone(parts);
            let HeldBytes::Disk(first) = &parts[0].payload else {
                continue;
            };
            let mut payload = first.clone();
            payload.len = parts.iter().map(|part| part.payload.len()).sum();
            // Concatenated gzip members preserve the full segment encoding.
            segment.gzip = parts
                .iter()
                .map(|part| match &part.gzip {
                    Some(HeldBytes::Disk(disk)) => Some(disk),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .map(|members| {
                    let mut gzip = members[0].clone();
                    gzip.len = members.iter().map(|member| member.len).sum();
                    HeldBytes::Disk(gzip)
                });
            segment.kind =
                StoredSegmentKind::Media(SegmentBody::Contiguous(HeldBytes::Disk(payload)));
            let bytes = segment_disk_bytes(&segment);
            resource.segment = Arc::new(segment);
            for part in parts.iter() {
                self.drop_part(part.id);
            }
            self.retained_disk_bytes = self.retained_disk_bytes.saturating_add(bytes);
        }
        self.chunked_spills.retain(|id| {
            self.segment_resources.get(id).is_some_and(|resource| {
                matches!(
                    resource.segment.kind,
                    StoredSegmentKind::Media(SegmentBody::Chunked(_))
                )
            })
        });
        if changed {
            self.refresh_published_segments();
        }
    }

    fn visible_playlist_duration(&self) -> Duration {
        self.visible_segments
            .iter()
            .filter_map(|id| self.segment_resources.get(id))
            .fold(Duration::ZERO, |total, resource| {
                total.saturating_add(
                    resource
                        .segment
                        .timebase
                        .ticks_to_duration(resource.segment.duration),
                )
            })
    }

    fn current_live_position(&self) -> Duration {
        let open_duration = self
            .open_segment
            .as_ref()
            .and_then(|open| {
                self.advertised_config
                    .map(|config| config.timebase.ticks_to_duration(open.duration))
            })
            .unwrap_or(Duration::ZERO);
        self.playlist_position.saturating_add(open_duration)
    }

    pub fn retained_payload_bytes(&self) -> usize {
        self.retained_payload_bytes
    }

    pub fn retained_disk_bytes(&self) -> usize {
        self.retained_disk_bytes
    }

    pub fn memory_resident_parts(&self) -> usize {
        self.part_resources
            .values()
            .filter(|resource| resource.part.payload.is_memory())
            .count()
    }

    pub fn memory_resident_segments(&self) -> usize {
        self.segment_resources
            .values()
            .filter(|resource| {
                matches!(
                    resource.segment.kind,
                    StoredSegmentKind::Gap | StoredSegmentKind::GapParts(_)
                ) || segment_holds_memory(&resource.segment)
            })
            .count()
    }

    pub fn forget_unreachable_initializations(&mut self) {
        // A replaced initialization can disappear only after every segment,
        // standalone part, and open segment that names it has gone. The
        // current initialization is retained even before its first media
        // object arrives, so publishing the header and then a chunk is safe.
        if self
            .initializations
            .iter()
            .all(|held| self.initialization_is_reachable(held.id))
        {
            return;
        }
        let removed_bytes: usize = self
            .initializations
            .iter()
            .filter(|held| !self.initialization_is_reachable(held.id))
            .map(|held| memory_len(held.payload.len(), held.gzip.as_ref()))
            .sum();
        let retained: Vec<_> = self
            .initializations
            .iter()
            .filter(|held| self.initialization_is_reachable(held.id))
            .cloned()
            .collect();
        self.initializations = retained.into();
        self.retained_payload_bytes = self.retained_payload_bytes.saturating_sub(removed_bytes);
    }

    /// Scheduled bytes still count as real RAM, but must not be scheduled twice.
    pub fn pending_spill_bytes(&self) -> usize {
        self.spilling.values().sum()
    }

    fn can_spill(&self, segment: &StoredSegment) -> bool {
        if !segment_holds_memory(segment) {
            return false;
        }
        match &segment.kind {
            // Keep advertised low-latency parts hot. Hidden part URIs can be
            // served from byte ranges in the segment during their fetch grace.
            StoredSegmentKind::Media(SegmentBody::Chunked(parts)) => parts.iter().all(|part| {
                self.part_resources
                    .get(&part.id)
                    .is_none_or(|resource| !resource.playlist_visible)
            }),
            _ => true,
        }
    }

    pub fn oldest_memory_spill_candidate(&self) -> Option<Instant> {
        self.retired_segments
            .iter()
            .chain(self.visible_segments.iter())
            .filter(|id| !self.spilling.contains_key(id))
            .find_map(|id| {
                let resource = self.segment_resources.get(id)?;
                self.can_spill(&resource.segment)
                    .then_some(resource.first_published_at)
            })
    }

    pub fn prepare_spill(&mut self) -> Option<(SegmentId, Vec<SpillObject>)> {
        let id = *self
            .retired_segments
            .iter()
            .chain(self.visible_segments.iter())
            .find(|id| {
                !self.spilling.contains_key(id)
                    && self
                        .segment_resources
                        .get(*id)
                        .is_some_and(|resource| self.can_spill(&resource.segment))
            })?;
        let resource = self.segment_resources.get(&id)?;
        let mut objects = Vec::new();
        match &resource.segment.kind {
            StoredSegmentKind::Media(SegmentBody::Contiguous(HeldBytes::Memory(payload))) => {
                objects.push(SpillObject {
                    kind: SpillKind::Segment,
                    payload: payload.clone(),
                    gzip: resource
                        .segment
                        .gzip
                        .as_ref()
                        .and_then(HeldBytes::as_memory)
                        .cloned(),
                });
            }
            StoredSegmentKind::Media(SegmentBody::Chunked(parts)) => {
                for part in parts.iter() {
                    let Some(payload) = part.payload.as_memory() else {
                        continue;
                    };
                    objects.push(SpillObject {
                        kind: SpillKind::Part(part.id),
                        payload: payload.clone(),
                        gzip: part.gzip.as_ref().and_then(HeldBytes::as_memory).cloned(),
                    });
                }
            }
            StoredSegmentKind::Media(SegmentBody::Contiguous(HeldBytes::Disk(_)))
            | StoredSegmentKind::Gap
            | StoredSegmentKind::GapParts(_) => return None,
        }
        if objects.is_empty() {
            return None;
        }
        let bytes = objects
            .iter()
            .map(|object| memory_len(object.payload.len(), object.gzip.as_ref()))
            .sum();
        self.spilling.insert(id, bytes);
        Some((id, objects))
    }

    pub fn abort_spill(&mut self, segment: SegmentId) {
        self.spilling.remove(&segment);
    }

    pub fn finish_spill(&mut self, outcome: &SpillOutcome) -> bool {
        if self.spilling.remove(&outcome.segment).is_none() {
            return false;
        }
        if !self.segment_resources.contains_key(&outcome.segment) {
            return false;
        }
        for object in &outcome.objects {
            match object.kind {
                SpillKind::Segment => self.spill_contiguous(outcome.segment, object),
                SpillKind::Part(id) => self.spill_part(id, object),
            }
        }
        self.relink_chunked_parent(outcome.segment);
        if self
            .segment_resources
            .get(&outcome.segment)
            .is_some_and(|resource| {
                matches!(
                    resource.segment.kind,
                    StoredSegmentKind::Media(SegmentBody::Chunked(_))
                )
            })
        {
            self.chunked_spills.push_back(outcome.segment);
        }
        self.refresh_published_segments();
        true
    }

    fn spill_contiguous(&mut self, segment: SegmentId, object: &super::disk::SpilledObject) {
        let Some(resource) = self.segment_resources.get_mut(&segment) else {
            return;
        };
        let mut stored = (*resource.segment).clone();
        if let StoredSegmentKind::Media(SegmentBody::Contiguous(held)) = &mut stored.kind {
            let memory = held.memory_bytes();
            self.retained_payload_bytes = self.retained_payload_bytes.saturating_sub(memory);
            self.retained_disk_bytes = self.retained_disk_bytes.saturating_add(object.payload.len);
            *held = HeldBytes::Disk(object.payload.clone());
        }
        if let Some(gzip) = &object.gzip {
            let prior = stored.gzip.as_ref().map_or(0, HeldBytes::memory_bytes);
            self.retained_payload_bytes = self.retained_payload_bytes.saturating_sub(prior);
            self.retained_disk_bytes = self.retained_disk_bytes.saturating_add(gzip.len);
            stored.gzip = Some(HeldBytes::Disk(gzip.clone()));
        }
        resource.segment = Arc::new(stored);
    }

    fn spill_part(&mut self, id: PartId, object: &super::disk::SpilledObject) {
        let Some(part_resource) = self.part_resources.get_mut(&id) else {
            return;
        };
        let mut part = (*part_resource.part).clone();
        let memory = part
            .payload
            .memory_bytes()
            .saturating_add(part.gzip.as_ref().map_or(0, HeldBytes::memory_bytes));
        self.retained_payload_bytes = self.retained_payload_bytes.saturating_sub(memory);
        self.retained_disk_bytes = self
            .retained_disk_bytes
            .saturating_add(object.payload.len + object.gzip.as_ref().map_or(0, |gzip| gzip.len));
        part.payload = HeldBytes::Disk(object.payload.clone());
        part.gzip = object.gzip.clone().map(HeldBytes::Disk);
        part_resource.part = Arc::new(part);
    }

    fn relink_chunked_parent(&mut self, id: SegmentId) {
        let Some(resource) = self.segment_resources.get(&id) else {
            return;
        };
        let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &resource.segment.kind else {
            return;
        };
        let part_ids: Vec<_> = parts.iter().map(|part| part.id).collect();
        let parts: Vec<_> = part_ids
            .into_iter()
            .filter_map(|part_id| {
                self.part_resources
                    .get(&part_id)
                    .map(|resource| Arc::clone(&resource.part))
            })
            .collect();
        let Some(resource) = self.segment_resources.get_mut(&id) else {
            return;
        };
        let mut stored = (*resource.segment).clone();
        stored.kind = StoredSegmentKind::Media(SegmentBody::Chunked(parts.into()));
        resource.segment = Arc::new(stored);
    }

    fn initialization_is_reachable(&self, id: InitializationId) -> bool {
        self.current_initialization == Some(id)
            || self
                .segment_resources
                .values()
                .any(|resource| resource.segment.initialization == id)
            || self
                .part_resources
                .values()
                .any(|resource| resource.part.initialization == id)
            || self
                .open_segment
                .as_ref()
                .is_some_and(|open| open.initialization == id)
    }

    fn advance_edge(&mut self, ended: bool) -> super::RenditionLiveEdge {
        let last_segment = self
            .visible_segments
            .back()
            .and_then(|id| self.segment_resources.get(id))
            .map(|resource| (resource.segment.msn, resource.segment.id));
        let last_part = self.part_order.back().and_then(|id| {
            self.part_resources
                .get(id)
                .map(|resource| (resource.part.cursor, resource.part.id))
        });
        let next_part_id = (!ended)
            .then_some(self.active_config)
            .flatten()
            .filter(|(_, config)| config.chunk_target.is_some())
            .map(|_| PartId(self.issued_parts.saturating_add(1)));
        let next = super::RenditionLiveEdge {
            last_iframe: self.next_iframe_msn.checked_sub(1),
            last_segment,
            last_part,
            next_part_id,
            ended,
            revision: self.live_edge.revision.saturating_add(1),
        };
        self.live_edge = next;
        next
    }
}

fn memory_len(payload_len: usize, gzip: Option<&Payload>) -> usize {
    payload_len.saturating_add(gzip.map_or(0, Payload::len))
}

fn segment_holds_memory(segment: &StoredSegment) -> bool {
    match &segment.kind {
        StoredSegmentKind::Media(SegmentBody::Contiguous(held)) => held.is_memory(),
        StoredSegmentKind::Media(SegmentBody::Chunked(parts)) => {
            parts.iter().any(|part| part.payload.is_memory())
        }
        StoredSegmentKind::Gap | StoredSegmentKind::GapParts(_) => false,
    }
}

fn unlink_segment_files(segment: &StoredSegment) {
    match &segment.kind {
        StoredSegmentKind::Media(SegmentBody::Contiguous(held)) => held.unlink_disk(),
        StoredSegmentKind::Media(SegmentBody::Chunked(_))
        | StoredSegmentKind::Gap
        | StoredSegmentKind::GapParts(_) => {}
    }
    if let Some(gzip) = &segment.gzip {
        gzip.unlink_disk();
    }
}
