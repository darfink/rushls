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

use std::{collections::HashMap, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use parking_lot::RwLock;
use tokio::{sync::watch, time::Instant};

use crate::{
    domain::RenditionId,
    mux::{PackagedMedia, PackagedPresentation, PackagingRenditionId},
};

use super::{
    PartId, PublicationAnchor, RenditionCatalogEntry, RenditionLiveEdge, RenditionSnapshot,
    ResolvedPresentation, ResolvedRenditionGroup, RetentionPolicy, SegmentId, StoreWriteError,
    StoredPart, StoredSegment, StreamSnapshot,
    rendition::{EdgeUpdate, RenditionState, notify_edges},
};

/// Live media for one logical stream, shared by publishers and readers.
#[derive(Debug)]
pub struct LiveStream {
    limits: RetentionPolicy,
    state: RwLock<StreamState>,
    /// Slow-changing request-facing topology and lifecycle state. Each
    /// rendition replaces its media snapshot independently.
    snapshot: ArcSwap<StreamSnapshot>,
}

#[derive(Debug)]
pub struct StreamState {
    renditions: Vec<RenditionState>,
    active_presentation: Option<ResolvedPresentation>,
    publication_anchors: Vec<PublicationAnchor>,
    issued_renditions: u32,
    /// Changes only for lifecycle, topology, or advertised metadata.
    catalog_revision: u64,
    ended: bool,
    /// Monotonic publisher generation and the current lease's identity.
    publication: u64,
    /// When the current publisher released its lease, if currently idle.
    idle_since: Option<Instant>,
    retained_payload_bytes: usize,
}

impl LiveStream {
    pub fn new(limits: RetentionPolicy) -> Self {
        let snapshot = StreamSnapshot {
            revision: 0,
            ended: false,
            idle: true,
            presentation: None,
            publication_anchors: Arc::from([]),
            renditions: Arc::from([]),
        };
        Self {
            limits,
            state: RwLock::new(StreamState {
                renditions: Vec::new(),
                active_presentation: None,
                publication_anchors: Vec::new(),
                issued_renditions: 0,
                catalog_revision: 0,
                ended: false,
                publication: 0,
                idle_since: Some(Instant::now()),
                retained_payload_bytes: 0,
            }),
            snapshot: ArcSwap::from_pointee(snapshot),
        }
    }

    pub fn revision(&self) -> u64 {
        self.snapshot.load().revision
    }

    pub fn is_ended(&self) -> bool {
        self.snapshot.load().ended
    }

    pub fn is_idle(&self) -> bool {
        self.snapshot.load().idle
    }

    pub fn current_publication(&self) -> u64 {
        self.state.read().publication
    }

    pub fn retained_payload_bytes(&self) -> usize {
        self.state.read().retained_payload_bytes
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
            .map(|rendition| rendition.snapshot())
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

    /// Waits until one rendition advances past `revision` or ends.
    ///
    /// This is the store primitive for LL-HLS blocking reload. Sibling
    /// renditions have independent watch channels, so one busy rendition does
    /// not spuriously wake requests for another.
    pub async fn wait_rendition_after(
        &self,
        rendition_id: RenditionId,
        revision: u64,
    ) -> Option<RenditionLiveEdge> {
        let mut updates = self.subscribe_rendition(rendition_id)?;
        loop {
            let current = *updates.borrow_and_update();
            if current.revision > revision || current.ended {
                return Some(current);
            }
            if updates.changed().await.is_err() {
                return Some(current);
            }
        }
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
            let existing = state.renditions.iter().position(|rendition| {
                !rendition.active
                    && !rendition.retired
                    && rendition.descriptor.compatible_with(descriptor)
            });
            let index = match existing {
                Some(index) => index,
                None => {
                    let rendition_id = RenditionId(state.issued_renditions);
                    state.issued_renditions = state.issued_renditions.saturating_add(1);
                    state
                        .renditions
                        .push(RenditionState::new(rendition_id, descriptor.clone()));
                    state.renditions.len() - 1
                }
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
        });
        state.prune_publication_anchors();
        state.recalculate_retained_bytes();
        state.bump_catalog();
        self.publish_catalog(&state);
        drop(state);

        notify_edges(edge_updates);
        (publication, mapping)
    }

    pub fn release(&self, publication: u64) {
        let mut state = self.state.write();
        if state.publication != publication || state.idle_since.is_some() {
            return;
        }
        state.idle_since = Some(Instant::now());
        let edge_updates: Vec<_> = state
            .renditions
            .iter_mut()
            // A retired rendition remains terminal while the stream waits for
            // reconnect; only active topology may resume publishing.
            .map(|rendition| {
                let ended = !rendition.active;
                rendition.commit(ended)
            })
            .collect();
        state.bump_catalog();
        self.publish_catalog(&state);
        drop(state);
        notify_edges(edge_updates);
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
        self.publish_catalog(&state);
        drop(state);

        notify_edges(edge_updates);
        true
    }

    pub fn sweep_expired(&self) {
        let mut state = self.state.write();
        state.sweep_expired(Instant::now());
    }

    pub fn write(
        &self,
        publication: u64,
        rendition_id: RenditionId,
        media: PackagedMedia,
    ) -> Result<bool, StoreWriteError> {
        let now = Instant::now();
        let mut state = self.state.write();
        if state.publication != publication {
            return Ok(false);
        }
        // Capacity reclamation precedes validation. Each affected rendition
        // publishes its cleanup immediately, so even a subsequently rejected
        // event cannot leave the request-facing cache pointing at expired
        // initialization state.
        state.sweep_expired(now);

        let existing = state
            .renditions
            .iter()
            .position(|rendition| rendition.rendition_id == rendition_id);
        let Some(index) = existing else {
            return Err(StoreWriteError::UnknownPackagingRendition {
                rendition_id: media.rendition_id(),
            });
        };
        let additional = state.renditions[index].additional_bytes_for(&media)?;
        let adds_part = matches!(&media, PackagedMedia::Chunk(_));
        let adds_segment = matches!(
            &media,
            PackagedMedia::Segment(_) | PackagedMedia::SegmentCompleted(_)
        );
        if adds_part && state.retained_parts() >= self.limits.maximum_parts {
            return Err(StoreWriteError::PartCapacityExceeded {
                maximum: self.limits.maximum_parts,
            });
        }
        if adds_segment && state.retained_segments() >= self.limits.maximum_segments {
            return Err(StoreWriteError::SegmentCapacityExceeded {
                maximum: self.limits.maximum_segments,
            });
        }
        if state
            .retained_payload_bytes
            .checked_add(additional)
            .is_none_or(|total| total > self.limits.maximum_payload_bytes)
        {
            return Err(StoreWriteError::PayloadCapacityExceeded {
                maximum: self.limits.maximum_payload_bytes,
                additional,
            });
        }

        let advertised_before = state.renditions[index].bitrate.snapshot().advertised();
        state.renditions[index].apply(publication, media, now, self.limits);
        state.renditions[index].forget_unreachable_initializations();
        state.recalculate_retained_bytes();
        let advertised_after = state.renditions[index].bitrate.snapshot().advertised();
        let update = state.renditions[index].commit(false);
        if advertised_before != advertised_after {
            state.bump_catalog();
            self.publish_catalog(&state);
        }
        drop(state);

        notify_edges([update]);
        Ok(true)
    }

    pub fn end(&self, publication: u64) -> bool {
        let now = Instant::now();
        let mut state = self.state.write();
        if state.publication != publication || state.ended {
            return false;
        }
        let edge_updates = state.end(now, self.limits);
        self.publish_catalog(&state);
        drop(state);

        notify_edges(edge_updates);
        true
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
        updates
    }

    fn bump_catalog(&mut self) {
        self.catalog_revision = self.catalog_revision.saturating_add(1);
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

    fn prune_publication_anchors(&mut self) {
        let current = self.publication;
        self.publication_anchors.retain(|anchor| {
            anchor.publication == current
                || self
                    .renditions
                    .iter()
                    .any(|rendition| rendition.references_publication(anchor.publication))
        });
    }

    fn recalculate_retained_bytes(&mut self) {
        self.retained_payload_bytes = self
            .renditions
            .iter()
            .map(RenditionState::retained_payload_bytes)
            .sum();
    }

    fn retained_parts(&self) -> usize {
        self.renditions
            .iter()
            .map(RenditionState::retained_parts)
            .sum()
    }

    fn retained_segments(&self) -> usize {
        self.renditions
            .iter()
            .map(RenditionState::retained_segments)
            .sum()
    }
}
