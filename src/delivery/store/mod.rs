//! Durable HLS media storage and request-facing publication snapshots.
//!
//! The process catalog and each rendition publish immutable state through
//! separate [`ArcSwap`](arc_swap::ArcSwap) values. A chunk rebuilds only its
//! rendition snapshot; topology, lifecycle, or multivariant attributes replace
//! the much smaller stream catalog. Consequently, viewer count does not
//! multiply snapshot construction: requests load and clone the latest
//! [`Arc`], while an older snapshot survives only for requests already using
//! it.
//!
//! [`RetentionPolicy`] is the single source of truth for playlist visibility,
//! standalone resource availability, and per-stream capacity. Retention
//! calculations remain in this module rather than leaking into playlist or HTTP
//! projection code.
//!
//! # Layout
//!
//! | Module | Owns |
//! |---|---|
//! | [`ids`] | Durable identities the store assigns |
//! | [`error`] | What a write may be refused for |
//! | [`contract`] | The playlist terms a rendition is frozen to |
//! | [`media`] | The retained media a projection reads |
//! | [`catalog`] | The slow-changing, request-facing stream view |
//! | [`rendition`] | One rendition's retention and live edge |
//! | [`stream`] | One logical stream across publications |
//! | [`retention`] | The deadlines all of the above obey |
//! | [`bitrate`] | Incremental segment-bitrate statistics |
//!
//! This module keeps only the process-wide entry points: the store itself and
//! the write lease a publisher holds against one stream.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use parking_lot::Mutex;

use crate::{
    domain::{Payload, RenditionId, StreamId},
    mux::{ClosedCaptionService, PackagedMedia, PackagedPresentation, PackagingRenditionId},
    observe::Events,
};

mod bitrate;
mod catalog;
mod contract;
mod disk;
mod error;
mod ids;
mod media;
mod rendition;
mod retention;
mod stream;

#[cfg(test)]
mod tests;

use catalog::RenditionView;
use disk::DiskTier;

pub use catalog::{
    PublicationAnchor, RenditionCatalogEntry, ResolvedPresentation, ResolvedRenditionGroup,
    StreamSnapshot,
};
pub use contract::PlaylistContract;
pub use disk::{DiskError, DiskLimits, DiskRef, HeldBytes};
pub use error::{StoreFull, StoreWriteError};
pub use ids::{InitializationId, Msn, PartCursor, PartId, PartIndex, SegmentId};
pub use media::{
    OpenSegment, PublishedSegments, RenditionBandwidth, RenditionBitrateStatistics,
    RenditionLiveEdge, RenditionSnapshot, SegmentBody, StoredInitialization, StoredPart,
    StoredSegment, StoredSegmentKind,
};
pub use retention::{
    DurationRule, MINIMUM_PLAYLIST_SEGMENTS, RetentionDepth, RetentionPolicy, RetentionTier,
    TargetDurationMultiple,
};
pub use stream::LiveStream;

/// Process-wide bounds and lifecycle policy for [`StreamStore`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreLimits {
    pub maximum_streams: usize,
    /// Cohesive HLS retention and capacity policy applied to each stream.
    pub retention: RetentionPolicy,
    /// Overflow directory and per-stream byte cap, or memory-only.
    pub disk: Option<DiskLimits>,
}

impl StoreLimits {
    /// How long an unleased stream stays available for a publisher to return.
    ///
    /// The same window its media is fetchable for, deliberately. A stream that
    /// still has playable media is one a viewer can still be watching, and
    /// retiring it early would end that playback; a stream whose media has all
    /// expired has nothing left to resume into. One window answers both, so
    /// there is no second retention concept to keep in step with this one.
    pub fn reconnect_window(&self) -> Duration {
        self.retention.retain
    }
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            maximum_streams: 1_024,
            retention: RetentionPolicy::default(),
            disk: None,
        }
    }
}

/// The process-wide set of streams available to viewers.
///
/// Cloning shares one store, so it can be handed to every ingest session and to
/// the HTTP layer at startup. Sessions publish into it as chunks close, while
/// readers load immutable cached snapshots. Serving therefore never reaches
/// into session state, and a session ending cannot invalidate a request already
/// in flight.
///
/// The outer [`ArcSwap`] makes request-path stream lookup lock-free; only rare
/// insertion and retirement clone the catalog.
///
/// A stream's lifetime is deliberately longer than any one publisher's. See
/// [`Self::lease`].
#[derive(Clone)]
pub struct StreamStore {
    streams: Arc<ArcSwap<HashMap<StreamId, Arc<LiveStream>>>>,
    mutations: Arc<Mutex<()>>,
    limits: StoreLimits,
    disk: Option<Arc<DiskTier>>,
    events: Events,
}

impl fmt::Debug for StreamStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamStore")
            .field("streams", &self.len())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Default for StreamStore {
    fn default() -> Self {
        Self::new(StoreLimits::default())
    }
}

impl StreamStore {
    pub fn new(limits: StoreLimits) -> Self {
        Self::try_new(limits).expect("disk tier is only opened when configured")
    }

    pub fn try_new(limits: StoreLimits) -> Result<Self, DiskError> {
        let disk = match &limits.disk {
            Some(disk) => Some(DiskTier::open(disk)?),
            None => None,
        };
        Ok(Self {
            streams: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            mutations: Arc::new(Mutex::new(())),
            limits,
            disk,
            events: Events::default(),
        })
    }

    /// Where capacity clipping and other stream facts are reported.
    ///
    /// Call before any lease: streams created earlier keep the destination
    /// they were born with.
    #[must_use]
    pub fn with_events(self, events: Events) -> Self {
        Self { events, ..self }
    }

    pub(crate) fn disk(&self) -> Option<&Arc<DiskTier>> {
        self.disk.as_ref()
    }

    pub fn limits(&self) -> &StoreLimits {
        &self.limits
    }

    /// Takes the write lease on a stream, creating it if nobody has yet.
    ///
    /// The stream itself is *not* owned by the lease. That separation makes
    /// takeover survivable: a replacement publisher inherits the existing
    /// [`LiveStream`], so durable numbering continues, retained media stays
    /// fetchable, and viewers never see a temporary catalog hole. Removing the
    /// stream with its publisher would reset sequence numbers and blank the
    /// stream during handover.
    ///
    /// Leasing revokes any outstanding lease. The displaced holder can still
    /// call [`StreamLease::write`], but its writes are dropped instead of being
    /// interleaved into its successor's media.
    pub fn lease(
        &self,
        stream: StreamId,
        presentation: &PackagedPresentation,
    ) -> Result<StreamLease, StoreFull> {
        let _mutation = self.mutations.lock();
        let current = self.streams.load_full();
        let live = if let Some(live) = current.get(&stream) {
            Arc::clone(live)
        } else {
            if current.len() >= self.limits.maximum_streams {
                return Err(StoreFull {
                    maximum: self.limits.maximum_streams,
                });
            }
            let live = Arc::new(LiveStream::with_events(
                stream.clone(),
                self.limits.retention,
                self.disk.clone(),
                self.events.clone(),
            ));
            live.attach_handle(Arc::downgrade(&live));
            let mut next = (*current).clone();
            next.insert(stream.clone(), Arc::clone(&live));
            self.streams.store(Arc::new(next));
            live
        };

        // Serialized with structural removal, so the returned lease can never
        // point at a stream that was retired between lookup and attachment.
        let (publication, renditions) = live.attach(presentation);
        Ok(StreamLease {
            stream,
            live,
            publication,
            renditions: Arc::new(renditions),
        })
    }

    /// Test-only lifecycle lease for metrics that do not publish media.
    #[cfg(test)]
    pub fn lease_without_presentation(&self, stream: StreamId) -> Result<StreamLease, StoreFull> {
        self.lease(
            stream,
            &PackagedPresentation {
                time_anchor: std::time::SystemTime::UNIX_EPOCH,
                renditions: Arc::from([]),
                groups: Arc::from([]),
                combinations: Arc::from([]),
                closed_captions: Arc::from([]),
            },
        )
    }

    pub fn get(&self, stream: &StreamId) -> Option<Arc<LiveStream>> {
        self.streams.load().get(stream).map(Arc::clone)
    }

    pub fn contains(&self, stream: &StreamId) -> bool {
        self.streams.load().contains_key(stream)
    }

    /// Streams retained for viewers, whether or not a publisher is attached.
    pub fn len(&self) -> usize {
        self.streams.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.load().is_empty()
    }

    /// Streams with a publisher attached right now.
    ///
    /// The difference from [`Self::len`] is the set waiting within their
    /// reconnect budget.
    pub fn leased(&self) -> usize {
        self.streams
            .load()
            .values()
            .filter(|live| !live.is_idle())
            .count()
    }

    /// Every retained stream, including idle ones still within `retain`.
    ///
    /// Idle streams are what a viewer can still fetch after the publisher
    /// left, so retention depth has to count them.
    pub fn live_streams(&self) -> Vec<(StreamId, Arc<LiveStream>)> {
        self.streams
            .load()
            .iter()
            .map(|(id, live)| (id.clone(), Arc::clone(live)))
            .collect()
    }

    /// Performs the store's periodic expiry and retirement work.
    ///
    /// The server should call this from one maintenance task. Cleanup is not
    /// hidden inside `StreamStore::new`, which may run outside a Tokio runtime
    /// and has no process-shutdown token. Writes still sweep before capacity
    /// checks so expired bytes cannot cause a false rejection.
    ///
    /// Reports streams whose reconnect window closed. Availability is claimed
    /// on the successful media commit instead: polling it here could observe a
    /// short publication only after its session had already ended.
    pub fn maintain(&self) -> Maintenance {
        let _mutation = self.mutations.lock();
        let current = self.streams.load_full();
        let mut next = None;
        let mut changed = Maintenance::default();

        for (stream, live) in current.iter() {
            live.sweep_expired();
            if live.retire_if_idle_for(self.limits.reconnect_window()) {
                // Only a stream viewers could reach becomes unreachable. One
                // that never served anything was never available to lose.
                if live.was_announced() {
                    changed.retired.push(stream.clone());
                }
                if let Some(disk) = &self.disk {
                    disk.remove_stream(stream, live.disk_epoch());
                }
                next.get_or_insert_with(|| (*current).clone())
                    .remove(stream);
            }
        }

        if let Some(next) = next {
            self.streams.store(Arc::new(next));
        }
        changed
    }
}

/// What one maintenance pass changed about stream reachability.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Maintenance {
    /// Streams whose reconnect window closed, and which were reachable before.
    pub retired: Vec<StreamId>,
}

/// A publisher's write lease on one stream.
///
/// Dropping it leaves the stream in place and idle, ready to be resumed. Ending
/// a stream is a separate, deliberate act—see [`Self::end`]—because “my
/// publisher went away” and “this stream is over” are different facts, and only
/// the second should stop viewers waiting for more media.
#[derive(Debug)]
pub struct StreamLease {
    stream: StreamId,
    live: Arc<LiveStream>,
    publication: u64,
    /// Routes muxer-local identities without leaking them into retained state.
    renditions: Arc<HashMap<PackagingRenditionId, RenditionId>>,
}

impl StreamLease {
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    pub fn live(&self) -> &Arc<LiveStream> {
        &self.live
    }

    /// Which publisher generation this lease writes as.
    pub fn publication(&self) -> u64 {
        self.publication
    }

    /// Wait before producing the next bounded mux batch. Writes remain
    /// synchronous so cancellation can still flush already accepted media.
    pub async fn ready(&self) -> Result<(), StoreWriteError> {
        self.live.ready(self.publication).await
    }

    /// Publishes one event, returning `false` if takeover revoked this lease.
    ///
    /// Revoked media is discarded rather than appended, so an incumbent
    /// flushing a stale tail cannot interleave it with successor media.
    pub fn write(&self, media: PackagedMedia) -> Result<bool, StoreWriteError> {
        self.write_encoded(media, None)
    }

    /// The same, carrying the gzip encoding delivery will serve this media in.
    ///
    /// Separate because deciding *which* media is worth encoding needs the
    /// packaging-to-media-type table, and the store is deliberately ignorant of
    /// it: a publisher answers that question and hands the result down, so this
    /// layer holds the bytes without knowing why they exist.
    pub fn write_encoded(
        &self,
        media: PackagedMedia,
        gzip: Option<Payload>,
    ) -> Result<bool, StoreWriteError> {
        let packaging_rendition_id = media.rendition_id();
        let Some(&rendition_id) = self.renditions.get(&packaging_rendition_id) else {
            return Err(StoreWriteError::UnknownPackagingRendition {
                rendition_id: packaging_rendition_id,
            });
        };
        self.live.write(self.publication, rendition_id, media, gzip)
    }

    /// Advertises the in-band caption services this publication carries.
    ///
    /// Returns whether the topology actually changed, so a caller can report a
    /// transition without tracking what it previously declared. A revoked lease
    /// cannot alter its successor's topology.
    pub fn declare_closed_captions(&self, services: Arc<[ClosedCaptionService]>) -> bool {
        self.live
            .declare_closed_captions(self.publication, services)
    }

    /// Marks the stream complete so readers stop waiting for new media.
    ///
    /// A revoked lease cannot end its successor's publication.
    pub fn end(&self) -> bool {
        self.live.end(self.publication)
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.live.release(self.publication);
    }
}
