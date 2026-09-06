//! One logical stream, shared by publishers and readers.
//!
//! A stream outlives any single publisher. Takeover and reconnect replace the
//! *publication* — a monotonic generation number — while durable numbering,
//! retained media, and the catalog entry viewers reach for all stay put. That
//! is the whole reason [`LiveStream`] is separate from the lease that writes
//! into it.
//!
//! Writes take the state lock, commit, publish, then release before waking
//! anyone. Reads never take it at all: they load an [`Arc`] from an
//! [`ArcSwap`] and are unaffected by whatever the writer does next.

use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use parking_lot::{RwLock, RwLockWriteGuard};
use tokio::{sync::watch, time::Instant};

use crate::{
    domain::{Payload, RenditionId, StreamId},
    mux::{ClosedCaptionService, PackagedMedia, PackagedPresentation, PackagingRenditionId},
    observe::{Events, RetentionClipReason, StreamEvent},
};

use super::{
    PartId, PlaylistContract, PublicationAnchor, RenditionCatalogEntry, RenditionLiveEdge,
    RenditionSnapshot, ResolvedPresentation, ResolvedRenditionGroup, RetentionDepth,
    RetentionPolicy, SegmentId, StoreWriteError, StoredPart, StoredSegment, StreamSnapshot,
    disk::{DiskTier, SpillJob, SpillObject, SpillOutcome},
    rendition::{EdgeUpdate, RenditionState, notify_edges},
};

/// Distinguishes two [`LiveStream`] values that reuse a public stream id.
///
/// After retirement the catalog may mint a new stream with the same name while
/// a spill job from the predecessor is still queued. Jobs carry this epoch so
/// their files cannot land in the successor's directory.
static NEXT_DISK_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Whether a mutation changed what the catalog says about the stream.
///
/// Separate from the media revision because the two move at different rates: a
/// chunk arrives many times per segment and never changes the catalog, and
/// republishing the whole topology for each one is the cost this distinction
/// exists to avoid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Catalog {
    /// Readers keep the snapshot they already hold.
    Unchanged,
    /// The snapshot is replaced. Its revisions are bumped by the mutation
    /// itself, which is the only place that knows which of the two moved.
    Republished,
}

/// Whether a mutation can affect an already-rendered media playlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Media {
    Changed,
    /// Nothing a media playlist renders was touched, so cached ones stand.
    Untouched,
}

/// Live media for one logical stream, shared by publishers and readers.
#[derive(Debug)]
pub struct LiveStream {
    id: StreamId,
    limits: RetentionPolicy,
    disk: Option<Arc<DiskTier>>,
    disk_capacity: usize,
    /// Process-wide identity for this stream's spill paths. Not the public id.
    disk_epoch: u64,
    this: OnceLock<Weak<LiveStream>>,
    events: Events,
    state: RwLock<StreamState>,
    /// Slow-changing request-facing topology and lifecycle state. Each
    /// rendition replaces its media snapshot independently.
    snapshot: ArcSwap<StreamSnapshot>,
    /// Exact invalidation epoch for every media-playlist-visible rendition
    /// snapshot change. Kept beside the catalog so a chunk does not rebuild it.
    media_revision: AtomicU64,
    /// Whether this stream has already been reported playable.
    ///
    /// Latched per *stream* rather than per publication, which is the whole
    /// point: a publisher reconnecting within the idle window resumes a stream
    /// viewers never lost, and announcing it again would say something untrue.
    announced: AtomicBool,
}

#[derive(Debug)]
pub struct StreamState {
    renditions: Vec<RenditionState>,
    active_presentation: Option<ResolvedPresentation>,
    publication_anchors: Vec<PublicationAnchor>,
    issued_renditions: u32,
    /// Changes only for lifecycle, topology, or advertised metadata.
    catalog_revision: u64,
    /// Catalog inputs consumed by media playlists. Bandwidth is deliberately
    /// excluded because it is rendered only by the multivariant playlist.
    media_catalog_revision: u64,
    ended: bool,
    /// Monotonic publisher generation and the current lease's identity.
    publication: u64,
    /// When the current publisher released its lease, if currently idle.
    idle_since: Option<Instant>,
    retained_payload_bytes: usize,
    retained_disk_bytes: usize,
    spill_failed: bool,
    /// Whether the advertised window is currently shorter than `retain`
    /// because a byte or object cap shed history. Latched so a high-bitrate
    /// publisher does not warn on every parent.
    capacity_clipped: bool,
}

impl LiveStream {
    pub fn new(id: StreamId, limits: RetentionPolicy, disk: Option<Arc<DiskTier>>) -> Self {
        Self::with_events(id, limits, disk, Events::default())
    }

    pub fn with_events(
        id: StreamId,
        limits: RetentionPolicy,
        disk: Option<Arc<DiskTier>>,
        events: Events,
    ) -> Self {
        let disk_capacity = disk.as_ref().map_or(0, |tier| tier.maximum_payload_bytes());
        let snapshot = StreamSnapshot {
            revision: 0,
            media_catalog_revision: 0,
            ended: false,
            idle: true,
            presentation: None,
            publication_anchors: Arc::from([]),
            renditions: Arc::from([]),
        };
        Self {
            id,
            limits,
            disk,
            disk_capacity,
            disk_epoch: NEXT_DISK_EPOCH.fetch_add(1, Ordering::Relaxed),
            this: OnceLock::new(),
            events,
            state: RwLock::new(StreamState {
                renditions: Vec::new(),
                active_presentation: None,
                publication_anchors: Vec::new(),
                issued_renditions: 0,
                catalog_revision: 0,
                media_catalog_revision: 0,
                ended: false,
                publication: 0,
                idle_since: Some(Instant::now()),
                retained_payload_bytes: 0,
                retained_disk_bytes: 0,
                spill_failed: false,
                capacity_clipped: false,
            }),
            snapshot: ArcSwap::from_pointee(snapshot),
            media_revision: AtomicU64::new(0),
            announced: AtomicBool::new(false),
        }
    }

    pub fn revision(&self) -> u64 {
        self.snapshot.load().revision
    }

    pub fn media_revision(&self) -> u64 {
        self.media_revision.load(Ordering::Acquire)
    }

    /// Whether a viewer asking now would be served media.
    ///
    /// A resolved presentation is not enough: one exists from the moment a
    /// publisher takes its lease, while the playlist it describes is still
    /// empty. What a viewer waits for is media at a live edge, which is the
    /// same thing delivery gates its first response on.
    ///
    /// Deliberately the most permissive reading — a part counts, not only a
    /// completed segment — so that this never claims a stream is playable
    /// later than it is. A node configured to withhold playlists until a
    /// segment completes makes a viewer wait slightly longer than this says.
    pub fn is_playable(&self) -> bool {
        self.snapshot.load().renditions.iter().any(|rendition| {
            let edge = &rendition.snapshot().live_edge;
            edge.last_segment.is_some() || edge.last_part.is_some()
        })
    }

    /// Claims the first-time transition to playable, latching it.
    ///
    /// Returns true exactly once per stream, for whoever should announce it.
    pub fn claim_availability(&self) -> bool {
        // Media writes call this for the lifetime of the stream. Once the
        // answer is known, avoid re-reading every rendition's live edge.
        if self.announced.load(Ordering::Relaxed) {
            return false;
        }
        self.is_playable() && !self.announced.swap(true, Ordering::Relaxed)
    }

    /// Whether this stream was ever announced playable, so its retirement is
    /// worth reporting. A stream that never served anything cannot stop.
    pub fn was_announced(&self) -> bool {
        self.announced.load(Ordering::Relaxed)
    }

    pub fn is_ended(&self) -> bool {
        self.snapshot.load().ended
    }

    pub fn is_idle(&self) -> bool {
        self.snapshot.load().idle
    }

    pub fn attach_handle(&self, this: Weak<Self>) {
        let _ = self.this.set(this);
    }

    pub fn retained_payload_bytes(&self) -> usize {
        self.state.read().retained_payload_bytes
    }

    pub fn retained_disk_bytes(&self) -> usize {
        self.state.read().retained_disk_bytes
    }

    pub fn disk_epoch(&self) -> u64 {
        self.disk_epoch
    }

    /// Configured retain versus what this stream currently advertises and holds.
    pub fn retention_depth(&self) -> RetentionDepth {
        let snapshot = self.snapshot();
        let held = snapshot
            .renditions
            .iter()
            .map(|entry| advertised_duration(&entry.snapshot()))
            .max()
            .unwrap_or(Duration::ZERO);
        RetentionDepth {
            requested: self.limits.retain,
            held,
            memory_bytes: self.retained_payload_bytes(),
            memory_capacity: self.limits.maximum_payload_bytes,
            disk_bytes: self.state.read().retained_disk_bytes,
            disk_capacity: self.disk_capacity,
        }
    }

    /// Returns the current immutable request-facing snapshot.
    ///
    /// Topology, lifecycle, and multivariant attributes replace this value.
    /// Ordinary chunks update only their rendition view. Requests increment an
    /// Arc refcount and never reconstruct snapshot vectors.
    pub fn snapshot(&self) -> Arc<StreamSnapshot> {
        self.snapshot.load_full()
    }

    pub fn rendition(&self, rendition_id: RenditionId) -> Option<Arc<RenditionSnapshot>> {
        self.snapshot
            .load()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .map(super::catalog::RenditionCatalogEntry::snapshot)
    }

    /// Reads one rendition's retained state under the shared lock.
    ///
    /// Every locked read wants the same two steps — find the rendition, look
    /// at one thing — and doing them separately is how a caller ends up
    /// holding the lock across work that does not need it.
    fn with_rendition<T>(
        &self,
        rendition_id: RenditionId,
        read: impl FnOnce(&RenditionState) -> T,
    ) -> Option<T> {
        self.state
            .read()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .map(read)
    }

    pub fn segment(
        &self,
        rendition_id: RenditionId,
        segment_id: SegmentId,
    ) -> Option<Arc<StoredSegment>> {
        let now = Instant::now();
        self.with_rendition(rendition_id, |rendition| rendition.segment(segment_id, now))?
    }

    pub fn part(&self, rendition_id: RenditionId, part_id: PartId) -> Option<Arc<StoredPart>> {
        let now = Instant::now();
        self.with_rendition(rendition_id, |rendition| rendition.part(part_id, now))?
    }

    pub fn rendition_live_edge(&self, rendition_id: RenditionId) -> Option<RenditionLiveEdge> {
        self.with_rendition(rendition_id, RenditionState::live_edge)
    }

    pub fn subscribe_rendition(
        &self,
        rendition_id: RenditionId,
    ) -> Option<watch::Receiver<RenditionLiveEdge>> {
        self.with_rendition(rendition_id, RenditionState::subscribe)
    }

    /// Ends a mutation: publishes what changed, then wakes whoever was waiting.
    ///
    /// Every mutating path finishes here rather than writing the sequence out
    /// itself, because the order is load-bearing in two directions and neither
    /// is visible from a call site. Both revisions have to be published while
    /// the write lock is still held, or two concurrent mutations can make their
    /// snapshots visible in the opposite order to the one they were applied in.
    /// The edge updates have to be announced after it is released, or a woken
    /// reader blocks on the very state it was woken to read.
    ///
    /// It also puts the [`Catalog`] decision in one place. The media revision
    /// advances unconditionally: it is what invalidates rendered playlists, and
    /// a mutation that skipped it would be served from the render cache until
    /// something unrelated moved.
    fn commit(
        &self,
        state: RwLockWriteGuard<'_, StreamState>,
        catalog: Catalog,
        edges: impl IntoIterator<Item = EdgeUpdate>,
    ) {
        self.commit_with(state, catalog, Media::Changed, edges);
    }

    /// The same, for a mutation that provably touches no media playlist.
    ///
    /// Only the multivariant playlist reads the catalog revision alone; every
    /// media playlist is keyed on the media revision as well. A change confined
    /// to presentation-wide attributes therefore has nothing to invalidate
    /// there, and bumping it would re-render every rendition's playlist to
    /// produce identical bytes.
    fn commit_with(
        &self,
        state: RwLockWriteGuard<'_, StreamState>,
        catalog: Catalog,
        media: Media,
        edges: impl IntoIterator<Item = EdgeUpdate>,
    ) {
        if catalog == Catalog::Republished {
            self.publish_catalog(&state);
        }
        if media == Media::Changed {
            self.advance_media_revision();
        }
        drop(state);

        notify_edges(edges);
    }

    fn publish_catalog(&self, state: &StreamState) {
        let renditions: Arc<[RenditionCatalogEntry]> = state
            .renditions
            .iter()
            .map(|rendition| RenditionCatalogEntry {
                rendition_id: rendition.rendition_id,
                active: rendition.active,
                key: rendition.descriptor.key.clone(),
                config: rendition.advertised_config,
                contract: rendition.contract,
                media: rendition.descriptor.media.clone(),
                codecs: Arc::clone(&rendition.descriptor.codecs),
                name: Arc::clone(&rendition.descriptor.name),
                language: rendition.descriptor.language.clone(),
                is_default: rendition.descriptor.is_default,
                declared_bandwidth: rendition.descriptor.declared_bandwidth,
                bandwidth: rendition.bitrate.snapshot().advertised(),
                view: Arc::clone(rendition.view()),
            })
            .collect::<Vec<_>>()
            .into();
        self.snapshot.store(Arc::new(StreamSnapshot {
            revision: state.catalog_revision,
            media_catalog_revision: state.media_catalog_revision,
            ended: state.ended,
            idle: state.idle_since.is_some(),
            presentation: state.active_presentation.clone(),
            publication_anchors: state.publication_anchors.clone().into(),
            renditions,
        }));
    }

    pub fn attach(
        &self,
        presentation: &PackagedPresentation,
    ) -> (u64, HashMap<PackagingRenditionId, RenditionId>) {
        let now = Instant::now();
        let mut state = self.state.write();
        let _ = state.sweep_expired(now);
        state.publication += 1;
        state.ended = false;
        state.idle_since = None;
        let publication = state.publication;
        state.publication_anchors.push(PublicationAnchor {
            publication,
            time_anchor: presentation.time_anchor,
        });
        let retention = self.limits;
        let mut edge_updates = Vec::new();

        for rendition in &mut state.renditions {
            rendition.finish_open_as_gap(now, retention);
            rendition.bitrate.break_contiguity();
            rendition.active_config = None;
            rendition.active = false;
            rendition.current_initialization = None;
            rendition.last_packaging_segment_id = None;
            rendition.forget_unreachable_initializations();
        }

        let mut mapping = HashMap::new();
        for descriptor in presentation.renditions.iter() {
            // Two independent gates. Packaging compatibility is the muxer's
            // notion of "the same output", and the playlist contract is this
            // layer's: a publication that would change EXT-X-TARGETDURATION or
            // PART-TARGET cannot continue an existing playlist even when the
            // muxer considers it a continuation, so it falls through to a new
            // durable rendition and the old one retires with an ENDLIST.
            let contract = PlaylistContract::derive(&descriptor.config);
            let existing = state.renditions.iter().position(|rendition| {
                !rendition.active
                    && !rendition.retired
                    && rendition.descriptor.compatible_with(descriptor)
                    && rendition.contract == contract
            });
            let index = if let Some(index) = existing {
                index
            } else {
                let rendition_id = RenditionId(state.issued_renditions);
                state.issued_renditions = state.issued_renditions.saturating_add(1);
                state
                    .renditions
                    .push(RenditionState::new(rendition_id, descriptor.clone()));
                state.renditions.len() - 1
            };
            let rendition = &mut state.renditions[index];
            rendition.descriptor = descriptor.clone();
            rendition.advertised_config = Some(descriptor.config);
            rendition.active_config = Some((publication, descriptor.config));
            rendition.active = true;
            rendition.retired = false;
            edge_updates.push(rendition.commit(false));
            mapping.insert(descriptor.packaging_rendition_id, rendition.rendition_id);
        }

        for rendition in state
            .renditions
            .iter_mut()
            .filter(|rendition| !rendition.active)
        {
            rendition.retired = true;
            edge_updates.push(rendition.commit(true));
        }

        let resolved_groups = presentation
            .groups
            .iter()
            .map(|group| ResolvedRenditionGroup {
                key: group.key.clone(),
                media_kind: group.media_kind,
                renditions: group
                    .renditions
                    .iter()
                    .filter_map(|rendition_id| mapping.get(rendition_id).copied())
                    .collect::<Vec<_>>()
                    .into(),
            })
            .collect::<Vec<_>>()
            .into();
        state.active_presentation = Some(ResolvedPresentation {
            time_anchor: presentation.time_anchor,
            groups: resolved_groups,
            combinations: Arc::clone(&presentation.combinations),
            closed_captions: Arc::clone(&presentation.closed_captions),
        });
        state.prune_publication_anchors();
        state.recalculate_retained_bytes();
        state.bump_catalog();
        state.bump_media_catalog();
        self.commit(state, Catalog::Republished, edge_updates);
        (publication, mapping)
    }

    /// Replaces the in-band caption services advertised by the live topology.
    ///
    /// Separate from [`Self::attach`] because captions are established by
    /// observing the bitstream, which necessarily happens after the
    /// presentation is published. Bumping the catalog revision is what
    /// re-renders the multivariant playlist. No media playlist mentions
    /// captions, so the media revision is deliberately left alone rather than
    /// invalidating every rendition's cached playlist to reproduce identical
    /// bytes.
    ///
    /// Ignored when the lease is not the current publisher, matching every
    /// other write path: a displaced publisher must not alter what viewers of
    /// its successor are told.
    pub fn declare_closed_captions(
        &self,
        publication: u64,
        services: Arc<[ClosedCaptionService]>,
    ) -> bool {
        let mut state = self.state.write();
        if state.publication != publication {
            return false;
        }
        let Some(presentation) = &state.active_presentation else {
            return false;
        };
        if presentation.closed_captions == services {
            return false;
        }
        state.active_presentation = Some(ResolvedPresentation {
            closed_captions: services,
            ..presentation.clone()
        });
        state.bump_catalog();
        self.commit_with(state, Catalog::Republished, Media::Untouched, []);
        true
    }

    pub fn release(&self, publication: u64) {
        let mut state = self.state.write();
        if state.publication != publication || state.idle_since.is_some() {
            return;
        }
        state.idle_since = Some(Instant::now());
        let stream_ended = state.ended;
        let edge_updates: Vec<_> = state
            .renditions
            .iter_mut()
            // Releasing the lease follows `end` during a clean shutdown, so it
            // must preserve that terminal edge rather than reopening active
            // renditions and restoring their preload hints.
            .map(|rendition| {
                let ended = stream_ended || !rendition.active;
                rendition.commit(ended)
            })
            .collect();
        state.bump_catalog();
        state.bump_media_catalog();
        self.commit(state, Catalog::Republished, edge_updates);
    }

    pub fn retire_if_idle_for(&self, duration: Duration) -> bool {
        let now = Instant::now();
        let mut state = self.state.write();
        let Some(idle_since) = state.idle_since else {
            return false;
        };
        if now.saturating_duration_since(idle_since) < duration {
            return false;
        }
        if state.ended {
            return true;
        }

        // Expiring the reconnect budget ends the stream on exactly the terms a
        // deliberate end uses. It may disappear from new catalog lookups, but
        // readers already holding its Arc still reach a terminal, internally
        // consistent state rather than waiting on media nobody will publish.
        let edge_updates = state.end(now, self.limits);
        self.commit(state, Catalog::Republished, edge_updates);
        true
    }

    pub fn sweep_expired(&self) {
        let mut state = self.state.write();
        if state.sweep_expired(Instant::now()) {
            self.advance_media_revision();
        }
    }

    pub fn write(
        &self,
        publication: u64,
        rendition_id: RenditionId,
        media: PackagedMedia,
        gzip: Option<Payload>,
    ) -> Result<bool, StoreWriteError> {
        let now = Instant::now();
        let mut state = self.state.write();
        if state.publication != publication {
            return Ok(false);
        }

        let existing = state
            .renditions
            .iter()
            .position(|rendition| rendition.rendition_id == rendition_id);
        let Some(index) = existing else {
            return Err(StoreWriteError::UnknownPackagingRendition {
                rendition_id: media.rendition_id(),
            });
        };
        // Capacity reclamation precedes validation. Only the rendition this
        // write lands in is swept here: a full sweep is O(media retained by
        // every rendition), while the maintenance tick already sweeps the
        // rest. The affected rendition still publishes its cleanup
        // immediately, so a subsequently rejected event cannot leave the
        // request-facing cache pointing at expired initialization state.
        if state.sweep_rendition(index, now) {
            self.advance_media_revision();
        }
        let additional = state.renditions[index].additional_bytes_for(&media, gzip.as_ref())?;
        let adds_part = matches!(&media, PackagedMedia::Chunk(_));
        let adds_segment = matches!(
            &media,
            PackagedMedia::Segment(_) | PackagedMedia::SegmentCompleted(_)
        );
        // Make room rather than refuse. Every budget here bounds *retention*,
        // and the only honest way to hold a bound while media keeps arriving
        // is to drop the oldest media rather than the newest — which is what
        // refusing amounted to, since a rejected write fails the session.
        //
        // At a long `retain` every one of these is reached in ordinary
        // operation: bytes first at a high bitrate, the segment count first at
        // a short cadence. All three therefore shed.
        let over_objects = |state: &StreamState| {
            (adds_part && state.memory_resident_parts() >= self.limits.maximum_parts)
                || (adds_segment
                    && state.memory_resident_segments() >= self.limits.maximum_segments)
        };
        let over_memory = |state: &StreamState| {
            state
                .retained_payload_bytes
                .checked_add(additional)
                .is_none_or(|total| total > self.limits.maximum_payload_bytes)
        };
        let over_disk = |state: &StreamState| {
            self.disk_capacity > 0 && state.retained_disk_bytes > self.disk_capacity
        };
        let spill_blocked = self.disk.is_none();
        let capacity_reason = |state: &StreamState| {
            if over_disk(state) {
                Some(RetentionClipReason::Disk)
            } else if over_memory(state) && spill_blocked {
                Some(RetentionClipReason::Memory)
            } else if over_objects(state) {
                Some(RetentionClipReason::Objects)
            } else {
                None
            }
        };
        let mut dropped_for = None;
        if capacity_reason(&state).is_some() {
            let mut reclaimed = state.sweep_expired(now);
            // Expiry may satisfy a limit. Report only the limit that still
            // requires dropping media after that ordinary cleanup.
            while let Some(reason) = capacity_reason(&state) {
                if !state.shed_oldest() {
                    break;
                }
                dropped_for.get_or_insert(reason);
                reclaimed = true;
            }
            if reclaimed {
                self.advance_media_revision();
            }
        }

        let advertised_before = state.renditions[index].bitrate.snapshot().advertised();
        state.renditions[index].apply(publication, media, gzip, now, self.limits);
        state.renditions[index].forget_unreachable_initializations();
        state.recalculate_retained_bytes();
        let advertised_after = state.renditions[index].bitrate.snapshot().advertised();
        let update = state.renditions[index].commit(false);
        let anchors_changed = state.prune_publication_anchors();
        let catalog = if advertised_before == advertised_after && !anchors_changed {
            Catalog::Unchanged
        } else {
            state.bump_catalog();
            Catalog::Republished
        };
        let clip = self.take_clip_event(&mut state, dropped_for);
        self.commit(state, catalog, [update]);
        self.emit_clip(clip);
        self.maybe_spill();
        Ok(true)
    }

    pub fn end(&self, publication: u64) -> bool {
        let now = Instant::now();
        let mut state = self.state.write();
        if state.publication != publication || state.ended {
            return false;
        }
        let edge_updates = state.end(now, self.limits);
        self.commit(state, Catalog::Republished, edge_updates);
        true
    }

    fn advance_media_revision(&self) {
        let _ =
            self.media_revision
                .fetch_update(Ordering::Release, Ordering::Relaxed, |revision| {
                    Some(revision.saturating_add(1))
                });
    }

    fn emit_clip(&self, event: Option<StreamEvent>) {
        if let Some(event) = event {
            self.events.stream(self.id.clone(), event);
        }
    }

    /// First time capacity, not `retain`, sized the advertised window.
    fn take_clip_event(
        &self,
        state: &mut StreamState,
        dropped_for: Option<RetentionClipReason>,
    ) -> Option<StreamEvent> {
        if dropped_for.is_none() && !state.capacity_clipped {
            return None;
        }
        // A longer sibling must not hide missing history in another rendition.
        // Empty renditions have no playable history to lose.
        let held = state
            .renditions
            .iter()
            .map(|rendition| advertised_duration(&rendition.view().snapshot()))
            .filter(|duration| !duration.is_zero())
            .min()
            .unwrap_or(Duration::ZERO);
        if held >= self.limits.retain {
            state.capacity_clipped = false;
            return None;
        }
        let reason = dropped_for?;
        if state.capacity_clipped {
            return None;
        }
        state.capacity_clipped = true;
        Some(StreamEvent::RetentionClipped {
            reason,
            requested: self.limits.retain,
            held,
        })
    }

    pub fn is_backpressured(&self) -> bool {
        if self.disk.is_none() {
            return false;
        }
        let state = self.state.read();
        state.spill_failed || state.retained_payload_bytes > self.limits.maximum_payload_bytes
    }

    /// Waits outside all store locks. The session calls this before consuming
    /// another sample, so RAM can overshoot by one bounded mux output batch.
    pub async fn ready(&self, publication: u64) -> Result<(), StoreWriteError> {
        let Some(disk) = &self.disk else {
            return Ok(());
        };
        loop {
            let notified = disk.progress().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            self.maybe_spill();
            {
                let state = self.state.read();
                if state.publication != publication {
                    return Ok(());
                }
                if state.spill_failed {
                    return Err(StoreWriteError::DiskSpillFailed);
                }
                if state.retained_payload_bytes <= self.limits.maximum_payload_bytes {
                    return Ok(());
                }
                // An undersized cap cannot prevent the open/advertised part
                // window from advancing. With no spillable history or worker
                // progress to await, allow that bounded live window to grow.
                let pending = state.pending_spill_bytes();
                let candidate = state
                    .renditions
                    .iter()
                    .any(|rendition| rendition.oldest_memory_spill_candidate().is_some());
                if pending == 0 && (!candidate || disk.spill_pending() == 0) {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub fn maybe_spill(&self) {
        let Some(disk) = &self.disk else {
            return;
        };
        let Some(weak) = self.this.get() else {
            return;
        };
        loop {
            let mut state = self.state.write();
            if state.spill_failed || disk.queue_is_full() {
                return;
            }
            let pending = state.pending_spill_bytes();
            if state.retained_payload_bytes.saturating_sub(pending)
                <= self.limits.maximum_payload_bytes
            {
                return;
            }
            let Some((rendition, segment, objects)) = state.take_spill_candidate() else {
                return;
            };
            let bytes: usize = objects
                .iter()
                .map(|object| object.payload.len() + object.gzip.as_ref().map_or(0, Payload::len))
                .sum();
            // Reserve disk space before submitting; worker lag must not make
            // several jobs all believe they own the same remaining capacity.
            if state
                .retained_disk_bytes
                .saturating_add(pending)
                .saturating_add(bytes)
                > self.disk_capacity
            {
                state
                    .renditions
                    .iter_mut()
                    .find(|item| item.rendition_id == rendition)
                    .expect("candidate rendition")
                    .abort_spill(segment);
                if state.shed_oldest() {
                    let clip = self.take_clip_event(&mut state, Some(RetentionClipReason::Disk));
                    self.advance_media_revision();
                    drop(state);
                    self.emit_clip(clip);
                    continue;
                }
                return;
            }
            let job = SpillJob {
                live: weak.clone(),
                stream: self.id.clone(),
                epoch: self.disk_epoch,
                rendition,
                segment,
                objects,
            };
            // try_enqueue never waits, so the reservation and enqueue remain
            // atomic with respect to other writers and worker completions.
            if !disk.try_enqueue(job) {
                state
                    .renditions
                    .iter_mut()
                    .find(|item| item.rendition_id == rendition)
                    .expect("candidate rendition")
                    .abort_spill(segment);
                return;
            }
        }
    }

    pub fn finish_spill(&self, outcome: &SpillOutcome) {
        let clip = {
            let mut state = self.state.write();
            let applied = state
                .renditions
                .iter_mut()
                .find(|item| item.rendition_id == outcome.rendition)
                .is_some_and(|rendition| rendition.finish_spill(outcome));
            if applied {
                if let Some(rendition) = state
                    .renditions
                    .iter_mut()
                    .find(|item| item.rendition_id == outcome.rendition)
                {
                    rendition.publish_snapshot();
                }
                state.recalculate_retained_bytes();
                // Spills enqueued before the disk counter caught up can land over
                // cap; treat that as the same full-disk backpressure as a write.
                let mut shed = false;
                while self.disk_capacity > 0
                    && state.retained_disk_bytes > self.disk_capacity
                    && state.shed_oldest()
                {
                    shed = true;
                }
                self.advance_media_revision();
                self.take_clip_event(&mut state, shed.then_some(RetentionClipReason::Disk))
            } else {
                super::disk::forget_spilled(&outcome.objects);
                None
            }
        };
        self.emit_clip(clip);
    }

    pub fn fail_spill(&self, rendition: RenditionId, segment: SegmentId) {
        self.abort_spill(rendition, segment);
        self.state.write().spill_failed = true;
    }

    pub fn abort_spill(&self, rendition: RenditionId, segment: SegmentId) {
        let mut state = self.state.write();
        if let Some(rendition) = state
            .renditions
            .iter_mut()
            .find(|item| item.rendition_id == rendition)
        {
            rendition.abort_spill(segment);
        }
    }
}

impl StreamState {
    /// Resolves every rendition to a terminal state and marks the stream over.
    ///
    /// Shared by a deliberate end and by an expired reconnect budget. The two
    /// differ only in what makes them legal, never in what they leave behind,
    /// and a viewer must not be able to tell which one happened.
    #[must_use = "the committed edges must be announced by notify_edges"]
    fn end(&mut self, now: Instant, retention: RetentionPolicy) -> Vec<EdgeUpdate> {
        let updates = self
            .renditions
            .iter_mut()
            .map(|rendition| {
                rendition.finish_open_as_gap(now, retention);
                rendition.commit(true)
            })
            .collect();
        self.ended = true;
        self.recalculate_retained_bytes();
        self.bump_catalog();
        self.bump_media_catalog();
        updates
    }

    fn bump_catalog(&mut self) {
        self.catalog_revision = self.catalog_revision.saturating_add(1);
    }

    fn bump_media_catalog(&mut self) {
        self.media_catalog_revision = self.media_catalog_revision.saturating_add(1);
    }

    fn sweep_expired(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for rendition in &mut self.renditions {
            let before = rendition.retained_object_counts();
            rendition.sweep_expired(now);
            rendition.forget_unreachable_initializations();
            if rendition.retained_object_counts() != before {
                rendition.publish_snapshot();
                changed = true;
            }
        }
        self.prune_publication_anchors();
        self.recalculate_retained_bytes();
        changed
    }

    /// Retires the oldest retired segment held by any rendition.
    ///
    /// Oldest across the whole stream rather than per rendition, so a stream
    /// whose renditions publish at different cadences sheds in publication
    /// order instead of unevenly truncating whichever one the write landed in.
    fn shed_oldest(&mut self) -> bool {
        let Some(index) = self
            .renditions
            .iter()
            .enumerate()
            .filter_map(|(index, rendition)| {
                rendition.oldest_shed_candidate().map(|at| (at, index))
            })
            .min()
            .map(|(_, index)| index)
        else {
            return false;
        };
        let shed = self.renditions[index].shed_oldest();
        if shed {
            self.renditions[index].publish_snapshot();
            self.recalculate_retained_bytes();
        }
        shed
    }

    /// Sweeps one rendition's expired resources, republishing its snapshot
    /// when anything was reclaimed.
    ///
    /// The write path uses this instead of [`Self::sweep_expired`] so one
    /// chunk costs work proportional to the rendition it lands in rather
    /// than to everything the stream retains. The maintenance tick still
    /// sweeps every rendition, and a refused capacity check falls back to a
    /// full sweep before the write is rejected.
    fn sweep_rendition(&mut self, index: usize, now: Instant) -> bool {
        let before = self.renditions[index].retained_object_counts();
        self.renditions[index].sweep_expired(now);
        self.renditions[index].forget_unreachable_initializations();
        let changed = self.renditions[index].retained_object_counts() != before;
        if changed {
            self.renditions[index].publish_snapshot();
            self.recalculate_retained_bytes();
        }
        changed
    }

    fn prune_publication_anchors(&mut self) -> bool {
        let before = self.publication_anchors.len();
        let current = self.publication;
        self.publication_anchors.retain(|anchor| {
            anchor.publication == current
                || self
                    .renditions
                    .iter()
                    .any(|rendition| rendition.references_publication(anchor.publication))
        });
        self.publication_anchors.len() != before
    }

    fn recalculate_retained_bytes(&mut self) {
        self.retained_payload_bytes = self
            .renditions
            .iter()
            .map(RenditionState::retained_payload_bytes)
            .sum();
        self.retained_disk_bytes = self
            .renditions
            .iter()
            .map(RenditionState::retained_disk_bytes)
            .sum();
    }

    fn memory_resident_parts(&self) -> usize {
        self.renditions
            .iter()
            .map(RenditionState::memory_resident_parts)
            .sum()
    }

    fn memory_resident_segments(&self) -> usize {
        self.renditions
            .iter()
            .map(RenditionState::memory_resident_segments)
            .sum()
    }

    fn pending_spill_bytes(&self) -> usize {
        self.renditions
            .iter()
            .map(RenditionState::pending_spill_bytes)
            .sum()
    }

    fn take_spill_candidate(&mut self) -> Option<(RenditionId, SegmentId, Vec<SpillObject>)> {
        let index = self
            .renditions
            .iter()
            .enumerate()
            .filter_map(|(index, rendition)| {
                rendition
                    .oldest_memory_spill_candidate()
                    .map(|at| (at, index))
            })
            .min()
            .map(|(_, index)| index)?;
        let rendition = &mut self.renditions[index];
        let rendition_id = rendition.rendition_id;
        rendition
            .prepare_spill()
            .map(|(segment, objects)| (rendition_id, segment, objects))
    }
}

/// Playlist media time a viewer walking this snapshot would see.
fn advertised_duration(snapshot: &RenditionSnapshot) -> Duration {
    let mut total = snapshot
        .segments
        .iter()
        .fold(Duration::ZERO, |total, segment| {
            total.saturating_add(segment.timebase.ticks_to_duration(segment.duration))
        });
    if let (Some(open), Some(config)) = (&snapshot.open_segment, snapshot.config) {
        total = total.saturating_add(config.timebase.ticks_to_duration(open.duration));
    }
    total
}
