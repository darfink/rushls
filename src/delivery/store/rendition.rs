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
    InitializationId, Msn, OpenSegment, PartCursor, PartId, PartIndex, PlaylistContract,
    PublishedSegments, RenditionBitrateStatistics, RenditionSnapshot, RenditionView,
    RetentionPolicy, SegmentBody, SegmentId, StoreWriteError, StoredInitialization, StoredPart,
    StoredSegment, StoredSegmentKind,
    bitrate::BitrateTracker,
    media::{segment_byte_len, segment_resource_bytes},
};

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
    /// Durable part-resource order. Hidden tags remain here while their
    /// standalone grace period or parent segment still needs their payload.
    part_order: VecDeque<PartId>,
    part_resources: HashMap<PartId, PartResource>,
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
    /// HLS numbering is independent from publisher-local packaging IDs and is
    /// never reset when a publisher reconnects.
    next_msn: u64,
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

impl RenditionState {
    pub fn new(rendition_id: RenditionId, descriptor: PackagedRendition) -> Self {
        let live_edge = super::RenditionLiveEdge::default();
        let (edge_updates, _) = watch::channel(live_edge);
        let contract = PlaylistContract::derive(&descriptor.config);
        let published = Arc::new(RenditionView::new(RenditionSnapshot {
            rendition_id,
            config: Some(descriptor.config),
            contract,
            media_sequence: 0,
            discontinuity_sequence: 0,
            initializations: Arc::from([]),
            segments: PublishedSegments::default(),
            open_segment: None,
            live_edge,
            bitrate: RenditionBitrateStatistics::default(),
        }));
        Self {
            rendition_id,
            advertised_config: Some(descriptor.config),
            active_config: None,
            descriptor,
            contract,
            active: false,
            retired: false,
            initializations: Arc::from([]),
            current_initialization: None,
            issued_initializations: 0,
            visible_segments: VecDeque::new(),
            published_segments: Arc::from([]),
            segment_resources: HashMap::new(),
            part_order: VecDeque::new(),
            part_resources: HashMap::new(),
            open_segment: None,
            retained_payload_bytes: 0,
            next_msn: 0,
            media_sequence: 0,
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

    pub fn retained_parts(&self) -> usize {
        self.part_resources.len()
    }

    pub fn retained_segments(&self) -> usize {
        self.segment_resources.len()
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
                .segment_resources
                .values()
                .any(|resource| resource.segment.publication == publication)
            || self
                .part_resources
                .values()
                .any(|resource| resource.part.publication == publication)
    }

    fn snapshot(&self) -> RenditionSnapshot {
        let visible = |part: &Arc<StoredPart>| {
            self.part_resources
                .get(&part.id)
                .is_some_and(|resource| resource.playlist_visible)
        };
        let parts_visible_from = self
            .part_order
            .iter()
            .filter_map(|id| self.part_resources.get(id))
            .find(|resource| resource.playlist_visible)
            .map(|resource| resource.part.cursor.msn);
        let open_segment = self.open_segment.as_ref().map(|open| {
            let mut open = open.clone();
            open.parts.retain(&visible);
            open
        });
        RenditionSnapshot {
            rendition_id: self.rendition_id,
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

    pub fn retained_object_counts(&self) -> (usize, usize, usize) {
        (
            self.part_resources.len(),
            self.segment_resources.len(),
            self.initializations.len(),
        )
    }

    pub fn additional_bytes_for(&self, media: &PackagedMedia) -> Result<usize, StoreWriteError> {
        self.validate(media)?;
        Ok(match media {
            PackagedMedia::Initialization(segment) => {
                if self.holds_current_initialization(segment) {
                    0
                } else {
                    segment.payload.len()
                }
            }
            PackagedMedia::Chunk(chunk) => chunk.payload.len(),
            PackagedMedia::Segment(segment) => segment.payload.len(),
            PackagedMedia::SegmentCompleted(_) => 0,
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
                if let Some(previous) = open.parts.last() {
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
        }
    }

    fn set_initialization(&mut self, segment: InitializationSegment, gzip: Option<Payload>) {
        // Initialization updates are rare, and repeating one is not an error:
        // a reconnecting publisher commonly re-sends the header it already
        // sent, and issuing a second ID for identical bytes would keep media
        // pointing at a section nothing distinguishes from the current one.
        if self.holds_current_initialization(&segment) {
            return;
        }
        let payload_bytes = segment.payload.len();
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
        let payload_bytes = chunk.payload.len();
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
            id,
            cursor,
            publication,
            initialization,
            media_start: chunk.media_start,
            duration: chunk.duration,
            timebase: config.timebase,
            independent: chunk.independent,
            payload: chunk.payload,
            gzip,
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
            id: SegmentId(self.issued_segments),
            msn: Msn(self.next_msn),
            publication,
            initialization,
            media_start: packaged.media_start,
            duration: packaged.duration,
            timebase: config.timebase,
            independent: packaged.independent,
            discontinuity_before: self.opens_discontinuity(publication),
            kind: StoredSegmentKind::Media(SegmentBody::Contiguous(packaged.payload)),
            gzip,
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

    fn insert_segment(&mut self, segment: StoredSegment, now: Instant, retention: RetentionPolicy) {
        let id = segment.id;
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

        let segment_target = self.advertised_config.map_or(Duration::MAX, |config| {
            config
                .timebase
                .ticks_to_duration(config.segment_target.get())
        });
        let minimum_playlist_duration = retention.minimum_playlist_duration_for(segment_target);
        let mut playlist_duration = self.visible_playlist_duration();
        while self.visible_segments.len() > retention.minimum_playlist_segments {
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
            resource.expires_at = retention.segment_fetch_deadline(
                now,
                segment_target,
                resource.first_published_at,
                removed_duration,
                resource.longest_playlist_duration,
            );
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
        self.published_segments = self
            .visible_segments
            .iter()
            .filter_map(|id| self.segment_resources.get(id))
            .map(|resource| Arc::clone(&resource.segment))
            .collect::<Vec<_>>()
            .into();
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
        self.bitrate.break_contiguity();
        for part in &open.parts {
            if let Some(resource) = self.part_resources.get_mut(&part.id) {
                resource.playlist_visible = false;
                resource.expires_at = retention.part_fetch_deadline(now, resource.segment_target);
            }
        }
        let duration = config.segment_target.get();
        let segment = StoredSegment {
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

        // A completed segment's PART tags are one description of that parent:
        // hiding a prefix makes its advertised start and duration contradict
        // EXTINF. Age the parent from its last part so the whole description
        // enters and leaves the playlist atomically.
        let hide_through = {
            let expired = |last_part: PartId| {
                let resource = self
                    .part_resources
                    .get(&last_part)
                    .expect("visible part was read from this map");
                let maximum_age = retention.part_tag_retention_for(resource.segment_target);
                live_position.saturating_sub(resource.playlist_end) > maximum_age
            };
            let mut current = None::<(Msn, PartId)>;
            let mut hidden = None;
            for id in &self.part_order {
                let Some(resource) = self
                    .part_resources
                    .get(id)
                    .filter(|resource| resource.playlist_visible)
                else {
                    continue;
                };
                let msn = resource.part.cursor.msn;
                if let Some((previous, last_part)) = current
                    && previous != msn
                {
                    if !expired(last_part) {
                        current = None;
                        break;
                    }
                    hidden = Some(previous);
                }
                current = Some((msn, *id));
            }
            if let Some((msn, last_part)) = current
                && expired(last_part)
            {
                hidden = Some(msn);
            }
            hidden
        };

        let Some(hide_through) = hide_through else {
            return;
        };
        for id in &self.part_order {
            let Some(resource) = self.part_resources.get_mut(id) else {
                continue;
            };
            if resource.part.cursor.msn > hide_through {
                break;
            }
            if !resource.playlist_visible {
                continue;
            }
            resource.playlist_visible = false;
            resource.expires_at = retention.part_fetch_deadline(now, resource.segment_target);
        }
    }

    pub fn sweep_expired(&mut self, now: Instant) {
        let part_resources = &mut self.part_resources;
        self.segment_resources.retain(|_, resource| {
            let expired = !resource.visible
                && resource
                    .expires_at
                    .is_some_and(|expires_at| now >= expires_at);
            if expired {
                // A chunked segment's bytes belong to its parts and are
                // released with them; only a contiguous segment holds bytes
                // of its own.
                self.retained_payload_bytes = self
                    .retained_payload_bytes
                    .saturating_sub(segment_resource_bytes(&resource.segment));
                if let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) =
                    &resource.segment.kind
                {
                    for part in parts.iter() {
                        if let Some(part_resource) = part_resources.get_mut(&part.id) {
                            part_resource.parent_retained = false;
                        }
                    }
                }
            }
            !expired
        });

        self.part_order.retain(|id| {
            let remove = self.part_resources.get(id).is_some_and(|resource| {
                !resource.parent_retained
                    && resource
                        .expires_at
                        .is_some_and(|expires_at| now >= expires_at)
            });
            if remove {
                let resource = self
                    .part_resources
                    .remove(id)
                    .expect("the part was inspected above");
                self.retained_payload_bytes = self
                    .retained_payload_bytes
                    .saturating_sub(resource.part.payload.len());
            }
            !remove
        });
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
            .map(|held| held.payload.len())
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
        let last_part = self
            .part_order
            .iter()
            .rev()
            .find_map(|id| {
                self.part_resources
                    .get(id)
                    .filter(|resource| resource.playlist_visible)
            })
            .map(|resource| (resource.part.cursor, resource.part.id));
        let next_part_id = (!ended)
            .then_some(self.active_config)
            .flatten()
            .filter(|(_, config)| config.chunk_target.is_some())
            .map(|_| PartId(self.issued_parts.saturating_add(1)));
        let next = super::RenditionLiveEdge {
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
