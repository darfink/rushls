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
    domain::RenditionId,
    mux::{
        InitializationSegment, PackagedChunk, PackagedMedia, PackagedRendition, PackagedSegment,
        PackagedSegmentCompletion, PackagingSegmentId, RenditionConfig,
    },
};

use super::{
    InitializationId, Msn, OpenSegment, PartCursor, PartId, PartIndex, RenditionBitrateStatistics,
    RenditionSnapshot, RenditionView, RetentionPolicy, SegmentBody, SegmentId, StoreWriteError,
    StoredInitialization, StoredPart, StoredSegment, StoredSegmentKind, bitrate::BitrateTracker,
    media::segment_byte_len,
};

/// A committed live edge and the channel that should announce it.
///
/// Sending is deferred until the caller has released the state lock, so a woken
/// reader never blocks behind the writer that woke it.
pub type EdgeUpdate = (watch::Sender<super::RenditionLiveEdge>, super::RenditionLiveEdge);

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
    initializations: Vec<StoredInitialization>,
    pub current_initialization: Option<InitializationId>,
    issued_initializations: u64,
    /// Segment IDs appearing in the current playlist window, in MSN order.
    visible_segments: VecDeque<SegmentId>,
    /// Downloadable segment resources, including entries no longer visible
    /// whose HLS availability deadline has not elapsed.
    segment_resources: HashMap<SegmentId, SegmentResource>,
    /// Durable part-resource order. Hidden tags remain here while their
    /// standalone grace period or parent segment still needs their payload.
    part_order: VecDeque<PartId>,
    part_resources: HashMap<PartId, PartResource>,
    /// At most one progressively published packaging segment per rendition.
    open_segment: Option<OpenSegment>,
    /// HLS numbering is independent from publisher-local packaging IDs and is
    /// never reset when a publisher reconnects.
    next_msn: u64,
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
        let published = Arc::new(RenditionView::new(RenditionSnapshot {
            rendition_id,
            config: Some(descriptor.config),
            initializations: Vec::new(),
            segments: Vec::new(),
            open_segment: None,
            live_edge,
            bitrate: RenditionBitrateStatistics::default(),
        }));
        Self {
            rendition_id,
            advertised_config: Some(descriptor.config),
            active_config: None,
            descriptor,
            active: false,
            retired: false,
            initializations: Vec::new(),
            current_initialization: None,
            issued_initializations: 0,
            visible_segments: VecDeque::new(),
            segment_resources: HashMap::new(),
            part_order: VecDeque::new(),
            part_resources: HashMap::new(),
            open_segment: None,
            next_msn: 0,
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
        let segments = self
            .visible_segments
            .iter()
            .filter_map(|id| self.segment_resources.get(id))
            .map(|resource| {
                let mut segment = (*resource.segment).clone();
                segment.parts.retain(&visible);
                segment
            })
            .collect();
        let open_segment = self.open_segment.as_ref().map(|open| {
            let mut open = open.clone();
            open.parts.retain(&visible);
            open
        });
        RenditionSnapshot {
            rendition_id: self.rendition_id,
            config: self.advertised_config,
            initializations: self.initializations.clone(),
            segments,
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

    pub fn additional_bytes_for(
        &self,
        media: &PackagedMedia,
    ) -> Result<usize, StoreWriteError> {
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
            PackagedMedia::Chunk(chunk) => {
                let config = self.require_media_ready()?;
                if config.chunk_target.is_none() {
                    return Err(StoreWriteError::ChunksDisabled { rendition_id });
                }
                match &self.open_segment {
                    Some(open) if open.packaging_segment_id != chunk.packaging_segment_id => {
                        return Err(StoreWriteError::DifferentSegmentAlreadyOpen {
                            rendition_id,
                            open: open.packaging_segment_id,
                            found: chunk.packaging_segment_id,
                        });
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
                        if open.media_start.checked_add_unsigned(open.duration)
                            != Some(chunk.media_start)
                        {
                            return Err(StoreWriteError::SegmentTimingMismatch { rendition_id });
                        }
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
                    }
                }
            }
            PackagedMedia::Segment(segment) => {
                let config = self.require_media_ready()?;
                if self.open_segment.is_some() {
                    return Err(StoreWriteError::DirectSegmentDuringOpenSegment { rendition_id });
                }
                if config.chunk_target.is_some() {
                    return Err(StoreWriteError::DirectSegmentsDisabled { rendition_id });
                }
                self.require_next_packaging_segment_id(segment.packaging_segment_id)?;
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
            }
        }
        Ok(())
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

    pub fn apply(
        &mut self,
        publication: u64,
        media: PackagedMedia,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        match media {
            PackagedMedia::Initialization(segment) => self.set_initialization(segment),
            PackagedMedia::Chunk(chunk) => self.push_chunk(publication, chunk, now, retention),
            PackagedMedia::Segment(segment) => {
                self.push_direct_segment(publication, segment, now, retention)
            }
            PackagedMedia::SegmentCompleted(completion) => {
                self.complete_segment(completion, now, retention)
            }
        }
    }

    fn set_initialization(&mut self, segment: InitializationSegment) {
        // Initialization updates are rare, and repeating one is not an error:
        // a reconnecting publisher commonly re-sends the header it already
        // sent, and issuing a second ID for identical bytes would keep media
        // pointing at a section nothing distinguishes from the current one.
        if self.holds_current_initialization(&segment) {
            return;
        }
        self.issued_initializations = self.issued_initializations.saturating_add(1);
        let id = InitializationId(self.issued_initializations);
        self.current_initialization = Some(id);
        self.initializations.push(StoredInitialization {
            id,
            version: segment.version,
            payload: segment.payload,
        });
    }

    fn push_chunk(
        &mut self,
        publication: u64,
        chunk: PackagedChunk,
        now: Instant,
        retention: RetentionPolicy,
    ) {
        let config = self.require_config().expect("validated configuration");
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
                parts: Vec::new(),
            });
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
            parts: parts.iter().cloned().collect(),
            kind: StoredSegmentKind::Media(SegmentBody::Chunked(Arc::clone(&parts))),
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
            parts: Vec::new(),
            kind: StoredSegmentKind::Media(SegmentBody::Contiguous(packaged.payload)),
        };
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

        let segment_target = self
            .advertised_config
            .map(|config| {
                config
                    .timebase
                    .ticks_to_duration(config.segment_target.get())
            })
            .unwrap_or(Duration::MAX);
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
        self.hide_old_parts(now, retention);
        self.sweep_expired(now);
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
            parts: Vec::new(),
            kind: StoredSegmentKind::Gap,
        };
        self.advance_playlist(
            open.packaging_segment_id,
            config.timebase.ticks_to_duration(duration),
        );
        self.insert_segment(segment, now, retention);
    }

    fn hide_old_parts(&mut self, now: Instant, retention: RetentionPolicy) {
        let live_position = self.current_live_position();
        for id in &self.part_order {
            let Some(resource) = self.part_resources.get_mut(id) else {
                continue;
            };
            if !resource.playlist_visible {
                continue;
            }
            let maximum_age = retention.part_tag_retention_for(resource.segment_target);
            if live_position.saturating_sub(resource.playlist_end) <= maximum_age {
                break;
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
            if expired
                && let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) =
                    &resource.segment.kind
            {
                for part in parts.iter() {
                    if let Some(part_resource) = part_resources.get_mut(&part.id) {
                        part_resource.parent_retained = false;
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
                self.part_resources.remove(id);
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
        let initialization_bytes: usize = self
            .initializations
            .iter()
            .map(|initialization| initialization.payload.len())
            .sum();
        let part_bytes: usize = self
            .part_resources
            .values()
            .map(|resource| resource.part.payload.len())
            .sum();
        let direct_segment_bytes: usize = self
            .segment_resources
            .values()
            .map(|resource| match &resource.segment.kind {
                StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) => payload.len(),
                StoredSegmentKind::Media(SegmentBody::Chunked(_)) | StoredSegmentKind::Gap => 0,
            })
            .sum();
        initialization_bytes
            .saturating_add(part_bytes)
            .saturating_add(direct_segment_bytes)
    }

    pub fn forget_unreachable_initializations(&mut self) {
        // A replaced initialization can disappear only after every segment,
        // standalone part, and open segment that names it has gone. The
        // current initialization is retained even before its first media
        // object arrives, so publishing the header and then a chunk is safe.
        let current = self.current_initialization;
        self.initializations.retain(|held| {
            Some(held.id) == current
                || self
                    .segment_resources
                    .values()
                    .any(|resource| resource.segment.initialization == held.id)
                || self
                    .part_resources
                    .values()
                    .any(|resource| resource.part.initialization == held.id)
                || self
                    .open_segment
                    .as_ref()
                    .is_some_and(|open| open.initialization == held.id)
        });
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
