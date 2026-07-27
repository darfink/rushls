//! Durable HLS media storage and request-facing publication snapshots.
//!
//! The process catalog and each rendition publish immutable state through
//! separate [`ArcSwap`] values. A chunk rebuilds only its rendition snapshot;
//! topology, lifecycle, or multivariant attributes replace the much smaller
//! stream catalog. Consequently, viewer count does not multiply snapshot
//! construction: requests load and clone the latest [`Arc`], while an older
//! snapshot survives only for requests already using it.
//!
//! [`RetentionPolicy`] is the single source of truth for playlist visibility,
//! standalone resource availability, and per-stream capacity. Retention
//! calculations remain in this module rather than leaking into playlist or HTTP
//! projection code.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
    time::{Duration, SystemTime},
};

use arc_swap::ArcSwap;
use derive_more::Display;
use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::{sync::watch, time::Instant};

use crate::{
    domain::{Payload, RenditionId, StreamId, TickDuration, TickTimestamp, Timebase},
    mux::{
        InitializationSegment, PackagedChunk, PackagedMedia, PackagedPresentation,
        PackagedRendition, PackagedSegment, PackagedSegmentCompletion, PackagingRenditionId,
        PackagingSegmentId, PlayableCombination, RenditionConfig, RenditionGroupKey, RenditionKey,
        RenditionMedia,
    },
};

mod bitrate;
mod retention;

use bitrate::BitrateTracker;
pub use retention::{DurationRule, RetentionPolicy, TargetDurationMultiple};

#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct InitializationId(pub u64);

#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct SegmentId(pub u64);

#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct PartId(pub u64);

#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct Msn(pub u64);

#[derive(Clone, Copy, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct PartIndex(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PartCursor {
    pub msn: Msn,
    pub part_index: PartIndex,
}

/// Process-wide bounds and lifecycle policy for [`StreamStore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreLimits {
    pub maximum_streams: usize,
    /// How long an unleased stream remains fetchable for reconnection.
    pub idle_retention: Duration,
    /// Cohesive HLS retention and capacity policy applied to each stream.
    pub retention: RetentionPolicy,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            maximum_streams: 1_024,
            idle_retention: Duration::from_secs(30),
            retention: RetentionPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the delivery store is already holding its maximum of {maximum} streams")]
pub struct StoreFull {
    pub maximum: usize,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoreWriteError {
    #[error("media references unknown {rendition_id}")]
    UnknownPackagingRendition { rendition_id: PackagingRenditionId },
    #[error("{rendition_id} is not active in the current publication")]
    RenditionInactive { rendition_id: RenditionId },
    #[error("media arrived for {rendition_id} before an initialization segment")]
    InitializationMissing { rendition_id: RenditionId },
    #[error("an initialization segment arrived while {rendition_id} had an open segment")]
    InitializationDuringOpenSegment { rendition_id: RenditionId },
    #[error(
        "{rendition_id} emitted packaging segment {found:?} after {previous:?}; \
         packaging segment identifiers must increase"
    )]
    NonMonotonicSegmentId {
        rendition_id: RenditionId,
        previous: PackagingSegmentId,
        found: PackagingSegmentId,
    },
    #[error("{rendition_id} emitted packaging segment {found:?} while {open:?} was still open")]
    DifferentSegmentAlreadyOpen {
        rendition_id: RenditionId,
        open: PackagingSegmentId,
        found: PackagingSegmentId,
    },
    #[error("{rendition_id} emitted chunk index {found} but the open segment expected {expected}")]
    UnexpectedChunkIndex {
        rendition_id: RenditionId,
        expected: u32,
        found: u32,
    },
    #[error("{rendition_id} completed a segment when none was open")]
    NoOpenSegment { rendition_id: RenditionId },
    #[error("{rendition_id} completed packaging segment {found:?}, but {open:?} was open")]
    WrongSegmentCompleted {
        rendition_id: RenditionId,
        open: PackagingSegmentId,
        found: PackagingSegmentId,
    },
    #[error("{rendition_id}'s completed segment timing does not match its chunks")]
    SegmentTimingMismatch { rendition_id: RenditionId },
    #[error("{rendition_id} emitted a direct segment while another segment was open")]
    DirectSegmentDuringOpenSegment { rendition_id: RenditionId },
    #[error("{rendition_id} emitted a chunk for a segment-only rendition")]
    ChunksDisabled { rendition_id: RenditionId },
    #[error("{rendition_id} emitted a direct segment for a chunked rendition")]
    DirectSegmentsDisabled { rendition_id: RenditionId },
    #[error(
        "retaining another {additional} bytes would exceed the stream payload budget of {maximum}"
    )]
    PayloadCapacityExceeded { maximum: usize, additional: usize },
    #[error("retaining another part would exceed the stream part limit of {maximum}")]
    PartCapacityExceeded { maximum: usize },
    #[error("retaining another segment would exceed the stream segment limit of {maximum}")]
    SegmentCapacityExceeded { maximum: usize },
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSegment {
    pub id: SegmentId,
    pub msn: Msn,
    pub publication: u64,
    pub initialization: InitializationId,
    pub packaging_segment_id: PackagingSegmentId,
    pub media_start: TickTimestamp,
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
    fn advertised(self) -> RenditionBandwidth {
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

/// Atomically published media-playlist state for one rendition.
///
/// The stream catalog retains one stable handle per rendition. Ordinary chunks
/// replace only this handle's latest snapshot, avoiding reconstruction of
/// sibling renditions or the slow-changing stream catalog. The store retains
/// only the latest value; an older value lives solely while an in-flight
/// request still holds its [`Arc`].
struct RenditionView {
    rendition_id: RenditionId,
    latest: ArcSwap<RenditionSnapshot>,
}

impl RenditionView {
    fn new(snapshot: RenditionSnapshot) -> Self {
        Self {
            rendition_id: snapshot.rendition_id,
            latest: ArcSwap::from_pointee(snapshot),
        }
    }

    /// Returns the already-built latest snapshot without cloning its vectors.
    fn snapshot(&self) -> Arc<RenditionSnapshot> {
        self.latest.load_full()
    }

    fn publish(&self, snapshot: RenditionSnapshot) {
        self.latest.store(Arc::new(snapshot));
    }
}

impl fmt::Debug for RenditionView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenditionView")
            .field("rendition_id", &self.rendition_id)
            .field("latest", &self.latest.load())
            .finish()
    }
}

/// Slow-changing attributes and the latest media snapshot for one rendition.
///
/// The advertised fields are captured immutably with the parent stream
/// revision, so a multivariant renderer cannot combine an old revision with
/// newer bitrate values during a concurrent segment publication.
#[derive(Clone, Debug)]
pub struct RenditionCatalogEntry {
    pub rendition_id: RenditionId,
    /// Only active entries participate in the current multivariant topology.
    /// Retired entries remain addressable while their media is retained.
    pub active: bool,
    pub key: RenditionKey,
    pub config: Option<RenditionConfig>,
    pub media: RenditionMedia,
    pub codecs: Arc<str>,
    pub name: Arc<str>,
    pub language: Option<Arc<str>>,
    pub is_default: bool,
    pub declared_bandwidth: Option<u64>,
    pub bandwidth: RenditionBandwidth,
    view: Arc<RenditionView>,
}

impl RenditionCatalogEntry {
    pub fn snapshot(&self) -> Arc<RenditionSnapshot> {
        self.view.snapshot()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRenditionGroup {
    pub key: RenditionGroupKey,
    pub media_kind: crate::domain::MediaKind,
    pub renditions: Arc<[RenditionId]>,
}

/// Active muxer-authored topology after publication-local IDs are resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPresentation {
    pub time_anchor: SystemTime,
    pub groups: Arc<[ResolvedRenditionGroup]>,
    pub combinations: Arc<[PlayableCombination]>,
}

/// Shared wall-clock origin for every rendition produced by one publication.
///
/// Segments retain their publication number, so a later playlist projection
/// can select the correct origin across reconnect discontinuities and advance
/// it using the segment's packaged ticks and timebase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationAnchor {
    pub publication: u64,
    pub time_anchor: SystemTime,
}

#[derive(Clone, Debug)]
pub struct StreamSnapshot {
    /// Changes only for lifecycle, rendition topology, or advertised metadata,
    /// including bandwidth values used by the multivariant projection.
    pub revision: u64,
    pub ended: bool,
    pub idle: bool,
    /// Current topology used for new multivariant projections.
    pub presentation: Option<ResolvedPresentation>,
    /// Wall-clock origins retained for media from older publications.
    pub publication_anchors: Arc<[PublicationAnchor]>,
    /// Immutable multivariant attributes plus stable handles through which
    /// renditions independently publish their latest media snapshots.
    pub renditions: Arc<[RenditionCatalogEntry]>,
}

impl StreamSnapshot {
    pub fn time_anchor(&self, publication: u64) -> Option<SystemTime> {
        self.publication_anchors
            .iter()
            .find(|anchor| anchor.publication == publication)
            .map(|anchor| anchor.time_anchor)
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
}

impl fmt::Debug for StreamStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamStore")
            .field("streams", &self.len())
            .field("limits", &self.limits)
            .finish()
    }
}

impl Default for StreamStore {
    fn default() -> Self {
        Self::new(StoreLimits::default())
    }
}

impl StreamStore {
    pub fn new(limits: StoreLimits) -> Self {
        Self {
            streams: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            mutations: Arc::new(Mutex::new(())),
            limits,
        }
    }

    pub fn limits(&self) -> StoreLimits {
        self.limits
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
        presentation: Arc<PackagedPresentation>,
    ) -> Result<StreamLease, StoreFull> {
        let _mutation = self.mutations.lock();
        let current = self.streams.load_full();
        let live = match current.get(&stream) {
            Some(live) => Arc::clone(live),
            None => {
                if current.len() >= self.limits.maximum_streams {
                    return Err(StoreFull {
                        maximum: self.limits.maximum_streams,
                    });
                }
                let live = Arc::new(LiveStream::new(self.limits.retention));
                let mut next = (*current).clone();
                next.insert(stream.clone(), Arc::clone(&live));
                self.streams.store(Arc::new(next));
                live
            }
        };

        // Serialized with structural removal, so the returned lease can never
        // point at a stream that was retired between lookup and attachment.
        let (publication, renditions) = live.attach(&presentation);
        Ok(StreamLease {
            stream,
            live,
            publication,
            renditions: Arc::new(renditions),
        })
    }

    /// Test-only lifecycle lease for metrics that do not publish media.
    #[cfg(test)]
    pub(crate) fn lease_without_presentation(
        &self,
        stream: StreamId,
    ) -> Result<StreamLease, StoreFull> {
        self.lease(
            stream,
            Arc::new(PackagedPresentation {
                time_anchor: SystemTime::UNIX_EPOCH,
                renditions: Arc::from([]),
                groups: Arc::from([]),
                combinations: Arc::from([]),
            }),
        )
    }

    pub fn get(&self, stream: &StreamId) -> Option<Arc<LiveStream>> {
        self.streams.load().get(stream).map(Arc::clone)
    }

    pub fn streams(&self) -> Vec<StreamId> {
        self.streams.load().keys().cloned().collect()
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

    /// Performs the store's periodic expiry and retirement work.
    ///
    /// The server should call this from one maintenance task. Cleanup is not
    /// hidden inside `StreamStore::new`, which may run outside a Tokio runtime
    /// and has no process-shutdown token. Writes still sweep before capacity
    /// checks so expired bytes cannot cause a false rejection.
    ///
    /// Returns the number of logical streams whose reconnect budget expired.
    pub fn maintain(&self) -> usize {
        let _mutation = self.mutations.lock();
        let current = self.streams.load_full();
        let mut next = None;
        let mut retired = 0;

        for (stream, live) in current.iter() {
            live.sweep_expired();
            if live.retire_if_idle_for(self.limits.idle_retention) {
                retired += 1;
                next.get_or_insert_with(|| (*current).clone())
                    .remove(stream);
            }
        }

        if let Some(next) = next {
            self.streams.store(Arc::new(next));
        }
        retired
    }
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

    /// True once another publisher has taken the stream over.
    pub fn is_revoked(&self) -> bool {
        self.live.current_publication() != self.publication
    }

    /// Publishes one event, returning `false` if takeover revoked this lease.
    ///
    /// Revoked media is discarded rather than appended, so an incumbent
    /// flushing a stale tail cannot interleave it with successor media.
    pub fn write(&self, media: PackagedMedia) -> Result<bool, StoreWriteError> {
        let packaging_rendition_id = media.rendition_id();
        let Some(&rendition_id) = self.renditions.get(&packaging_rendition_id) else {
            return Err(StoreWriteError::UnknownPackagingRendition {
                rendition_id: packaging_rendition_id,
            });
        };
        self.live.write(self.publication, rendition_id, media)
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
struct StreamState {
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

#[derive(Debug)]
struct RenditionState {
    /// Stable logical identity used to reconnect packaging output to the same
    /// media-playlist projection across publisher takeovers.
    rendition_id: RenditionId,
    /// Muxer-authored identity and attributes from the latest compatible
    /// publication. This remains available while retired media is fetchable.
    descriptor: PackagedRendition,
    active: bool,
    /// Once removed from an active topology, this playlist remains terminal.
    /// Reusing it later would make an ENDLIST disappear for existing viewers.
    retired: bool,
    /// Last configuration advertised for this rendition. It survives a
    /// publisher so topology remains available while the stream is idle.
    advertised_config: Option<RenditionConfig>,
    /// Configuration admitted for the current publisher generation.
    active_config: Option<(u64, RenditionConfig)>,
    /// Initialization sections remain while current or retained media
    /// references them. Payload clones are refcounts, not byte copies.
    initializations: Vec<StoredInitialization>,
    current_initialization: Option<InitializationId>,
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
    open_segment: Option<OpenSegmentState>,
    /// HLS numbering is independent from publisher-local packaging IDs and is
    /// never reset when a publisher reconnects.
    next_msn: u64,
    issued_segments: u64,
    issued_parts: u64,
    last_packaging_segment_id: Option<PackagingSegmentId>,
    /// Media-time position at the end of completed playlist segments.
    playlist_position: Duration,
    bitrate: BitrateTracker,
    /// Authoritative committed edge. The watch sender mirrors it only after
    /// the state lock is released.
    live_edge: RenditionLiveEdge,
    edge_updates: watch::Sender<RenditionLiveEdge>,
    /// Stable request-facing handle. Only this rendition's latest snapshot is
    /// replaced when its media advances.
    published: Arc<RenditionView>,
}

#[derive(Debug)]
struct OpenSegmentState {
    id: SegmentId,
    msn: Msn,
    publication: u64,
    initialization: InitializationId,
    packaging_segment_id: PackagingSegmentId,
    media_start: TickTimestamp,
    parts: Vec<Arc<StoredPart>>,
    duration: TickDuration,
}

#[derive(Debug)]
struct SegmentResource {
    segment: Arc<StoredSegment>,
    visible: bool,
    /// Monotonic publication time used as the live-playlist availability
    /// deadline anchor required by HLS.
    first_published_at: Instant,
    /// Largest playlist duration observed while this segment was present.
    /// HLS defines segment availability using this historical maximum.
    longest_playlist_duration: Duration,
    expires_at: Option<Instant>,
}

#[derive(Debug)]
struct PartResource {
    part: Arc<StoredPart>,
    playlist_end: Duration,
    segment_target: Duration,
    playlist_visible: bool,
    expires_at: Option<Instant>,
    parent_retained: bool,
}

impl LiveStream {
    fn new(limits: RetentionPolicy) -> Self {
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
    /// Ordinary chunks update only their [`RenditionView`]. Requests increment
    /// an Arc refcount and never reconstruct snapshot vectors.
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

    pub fn segment(
        &self,
        rendition_id: RenditionId,
        segment_id: SegmentId,
    ) -> Option<Arc<StoredSegment>> {
        let now = Instant::now();
        self.state
            .read()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .and_then(|rendition| rendition.segment_resources.get(&segment_id))
            .filter(|resource| {
                resource
                    .expires_at
                    .is_none_or(|expires_at| now < expires_at)
            })
            .map(|resource| Arc::clone(&resource.segment))
    }

    pub fn part(&self, rendition_id: RenditionId, part_id: PartId) -> Option<Arc<StoredPart>> {
        let now = Instant::now();
        self.state
            .read()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .and_then(|rendition| rendition.part_resources.get(&part_id))
            .filter(|resource| {
                resource
                    .expires_at
                    .is_none_or(|expires_at| now < expires_at)
            })
            .map(|resource| Arc::clone(&resource.part))
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
                view: Arc::clone(&rendition.published),
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

    pub fn rendition_live_edge(&self, rendition_id: RenditionId) -> Option<RenditionLiveEdge> {
        self.state
            .read()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .map(|rendition| rendition.live_edge)
    }

    pub fn subscribe_rendition(
        &self,
        rendition_id: RenditionId,
    ) -> Option<watch::Receiver<RenditionLiveEdge>> {
        self.state
            .read()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .map(|rendition| rendition.edge_updates.subscribe())
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

    fn attach(
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
            let edge = rendition.advance_edge(false);
            rendition.publish_snapshot();
            edge_updates.push((rendition.edge_updates.clone(), edge));
            mapping.insert(descriptor.packaging_rendition_id, rendition.rendition_id);
        }

        for rendition in state
            .renditions
            .iter_mut()
            .filter(|rendition| !rendition.active)
        {
            rendition.retired = true;
            let edge = rendition.advance_edge(true);
            rendition.publish_snapshot();
            edge_updates.push((rendition.edge_updates.clone(), edge));
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

    fn release(&self, publication: u64) {
        let mut state = self.state.write();
        if state.publication != publication || state.idle_since.is_some() {
            return;
        }
        state.idle_since = Some(Instant::now());
        let edge_updates = state
            .renditions
            .iter_mut()
            .map(|rendition| {
                // A retired rendition remains terminal while the stream waits
                // for reconnect; only active topology may resume publishing.
                let edge = rendition.advance_edge(!rendition.active);
                rendition.publish_snapshot();
                (rendition.edge_updates.clone(), edge)
            })
            .collect();
        state.bump_catalog();
        self.publish_catalog(&state);
        drop(state);
        notify_edges(edge_updates);
    }

    fn retire_if_idle_for(&self, duration: Duration) -> bool {
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

        let retention = self.limits;
        let mut edge_updates = Vec::new();
        for rendition in &mut state.renditions {
            // Expiring the reconnect budget resolves any observed open MSN as
            // a gap before waking blocked readers. The stream may disappear
            // from new catalog lookups, but readers already holding its Arc
            // still receive a terminal, internally consistent state.
            rendition.finish_open_as_gap(now, retention);
            let edge = rendition.advance_edge(true);
            rendition.publish_snapshot();
            edge_updates.push((rendition.edge_updates.clone(), edge));
        }
        state.ended = true;
        state.recalculate_retained_bytes();
        state.bump_catalog();
        self.publish_catalog(&state);
        drop(state);

        notify_edges(edge_updates);
        true
    }

    fn sweep_expired(&self) {
        let mut state = self.state.write();
        state.sweep_expired(Instant::now());
    }

    fn write(
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

        let bitrate_before = state.renditions[index].bitrate.snapshot();
        state.renditions[index].apply(publication, media, now, self.limits)?;
        state.renditions[index].forget_unreachable_initializations();
        state.recalculate_retained_bytes();
        let catalog_changed =
            advertised_bitrate_changed(bitrate_before, state.renditions[index].bitrate.snapshot());
        let sender = state.renditions[index].edge_updates.clone();
        let edge = state.renditions[index].advance_edge(false);
        state.renditions[index].publish_snapshot();
        if catalog_changed {
            state.bump_catalog();
            self.publish_catalog(&state);
        }
        drop(state);

        notify_edges(vec![(sender, edge)]);
        Ok(true)
    }

    fn end(&self, publication: u64) -> bool {
        let now = Instant::now();
        let mut state = self.state.write();
        if state.publication != publication || state.ended {
            return false;
        }
        let retention = self.limits;
        let mut edge_updates = Vec::new();
        for rendition in &mut state.renditions {
            rendition.finish_open_as_gap(now, retention);
            let edge = rendition.advance_edge(true);
            rendition.publish_snapshot();
            edge_updates.push((rendition.edge_updates.clone(), edge));
        }
        state.ended = true;
        state.recalculate_retained_bytes();
        state.bump_catalog();
        self.publish_catalog(&state);
        drop(state);

        notify_edges(edge_updates);
        true
    }
}

impl StreamState {
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
            .map(|rendition| rendition.part_resources.len())
            .sum()
    }

    fn retained_segments(&self) -> usize {
        self.renditions
            .iter()
            .map(|rendition| rendition.segment_resources.len())
            .sum()
    }
}

impl RenditionState {
    fn references_publication(&self, publication: u64) -> bool {
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

    fn new(rendition_id: RenditionId, descriptor: PackagedRendition) -> Self {
        let live_edge = RenditionLiveEdge::default();
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

    fn snapshot(&self) -> RenditionSnapshot {
        let segments = self
            .visible_segments
            .iter()
            .filter_map(|id| self.segment_resources.get(id))
            .map(|resource| {
                let mut segment = (*resource.segment).clone();
                segment.parts.retain(|part| {
                    self.part_resources
                        .get(&part.id)
                        .is_some_and(|resource| resource.playlist_visible)
                });
                segment
            })
            .collect();
        let open_segment = self.open_segment.as_ref().map(|open| OpenSegment {
            id: open.id,
            msn: open.msn,
            publication: open.publication,
            initialization: open.initialization,
            packaging_segment_id: open.packaging_segment_id,
            media_start: open.media_start,
            parts: open
                .parts
                .iter()
                .filter(|part| {
                    self.part_resources
                        .get(&part.id)
                        .is_some_and(|resource| resource.playlist_visible)
                })
                .cloned()
                .collect(),
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

    fn publish_snapshot(&self) {
        self.published.publish(self.snapshot());
    }

    fn retained_object_counts(&self) -> (usize, usize, usize) {
        (
            self.part_resources.len(),
            self.segment_resources.len(),
            self.initializations.len(),
        )
    }

    fn additional_bytes_for(&self, media: &PackagedMedia) -> Result<usize, StoreWriteError> {
        self.validate(media)?;
        Ok(match media {
            PackagedMedia::Initialization(segment) => {
                if self.current_initialization.is_some_and(|id| {
                    self.initializations.iter().any(|held| {
                        held.id == id
                            && held.version == segment.version
                            && held.payload == segment.payload
                    })
                }) {
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

    fn apply(
        &mut self,
        publication: u64,
        media: PackagedMedia,
        now: Instant,
        retention: RetentionPolicy,
    ) -> Result<(), StoreWriteError> {
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
        Ok(())
    }

    fn set_initialization(&mut self, segment: InitializationSegment) {
        // Initialization updates are rare. Equality checks rendition and
        // version before payload bytes, and avoids allocating a second durable
        // ID when a muxer repeats an identical header.
        if let Some(current) = self.current_initialization
            && self.initializations.iter().any(|held| {
                held.id == current
                    && held.version == segment.version
                    && held.payload == segment.payload
            })
        {
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
            self.open_segment = Some(OpenSegmentState {
                id: SegmentId(self.issued_segments),
                msn: Msn(self.next_msn),
                publication,
                initialization,
                packaging_segment_id: chunk.packaging_segment_id,
                media_start: chunk.media_start,
                parts: Vec::new(),
                duration: 0,
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
        let body = SegmentBody::Chunked(Arc::clone(&parts));
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
            kind: StoredSegmentKind::Media(body),
        };
        self.last_packaging_segment_id = Some(completion.packaging_segment_id);
        self.next_msn = self.next_msn.saturating_add(1);
        let duration = config.timebase.ticks_to_duration(completion.duration);
        let bytes = segment_byte_len(&segment);
        self.playlist_position = self.playlist_position.saturating_add(duration);
        self.bitrate.observe(
            bytes,
            duration,
            config
                .timebase
                .ticks_to_duration(config.segment_target.get()),
        );
        self.insert_segment(segment, now, retention);
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
        self.last_packaging_segment_id = Some(packaged.packaging_segment_id);
        self.next_msn = self.next_msn.saturating_add(1);
        let duration = config.timebase.ticks_to_duration(packaged.duration);
        let bytes = segment_byte_len(&segment);
        self.playlist_position = self.playlist_position.saturating_add(duration);
        self.bitrate.observe(
            bytes,
            duration,
            config
                .timebase
                .ticks_to_duration(config.segment_target.get()),
        );
        self.insert_segment(segment, now, retention);
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

    fn finish_open_as_gap(&mut self, now: Instant, retention: RetentionPolicy) {
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
        self.last_packaging_segment_id = Some(open.packaging_segment_id);
        self.next_msn = self.next_msn.saturating_add(1);
        self.playlist_position = self
            .playlist_position
            .saturating_add(config.timebase.ticks_to_duration(duration));
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

    fn sweep_expired(&mut self, now: Instant) {
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

    fn retained_payload_bytes(&self) -> usize {
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

    fn forget_unreachable_initializations(&mut self) {
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

    fn advance_edge(&mut self, ended: bool) -> RenditionLiveEdge {
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
        let next = RenditionLiveEdge {
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

fn segment_byte_len(segment: &StoredSegment) -> usize {
    match &segment.kind {
        StoredSegmentKind::Media(body) => body.len(),
        StoredSegmentKind::Gap => 0,
    }
}

fn advertised_bitrate_changed(
    before: RenditionBitrateStatistics,
    after: RenditionBitrateStatistics,
) -> bool {
    before.advertised() != after.advertised()
}

fn notify_edges(updates: Vec<(watch::Sender<RenditionLiveEdge>, RenditionLiveEdge)>) {
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

#[cfg(test)]
mod tests {
    use std::{num::NonZero, sync::Arc, time::SystemTime};

    use crate::{
        admission::StreamPolicy,
        domain::{
            Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters, TrackCatalog, TrackId,
        },
        media::validate,
        mux::{
            MediaSegmentFormat, PackagedChunk, PackagedPresentation, PackagedRendition,
            PackagedSegment, PackagedSegmentCompletion, PlayableCombination, RenditionGroup,
            RenditionGroupKey, RenditionKey, RenditionMedia,
        },
    };

    use super::*;

    fn stream() -> StreamId {
        StreamId::new("live/camera")
    }

    fn limits() -> StoreLimits {
        StoreLimits {
            maximum_streams: 8,
            idle_retention: Duration::from_secs(30),
            retention: RetentionPolicy::default(),
        }
    }

    fn store() -> StreamStore {
        StreamStore::new(limits())
    }

    fn packaged_presentation(configs: &[(u32, bool)]) -> Arc<PackagedPresentation> {
        let catalog = TrackCatalog::new(vec![DiscoveredTrack {
            id: TrackId(0),
            source_key: None,
            codec: Codec::H264,
            parameters: MediaParameters::Video {
                width: nz::u32!(1920),
                height: nz::u32!(1080),
                frame_rate: Some(FrameRate::new(nz::u32!(30), nz::u32!(1))),
                video_delay: 0,
            },
            timebase: Timebase::new(nz::u32!(1), nz::u32!(1)),
            first_pts: Some(0),
            title: None,
            language: None,
            codec_extradata: Payload::default(),
        }])
        .expect("test catalog is valid");
        let input = validate(&catalog, &StreamPolicy::permissive()).expect("test input is valid");
        let renditions: Vec<_> = configs
            .iter()
            .map(|&(rendition, chunked)| PackagedRendition {
                packaging_rendition_id: PackagingRenditionId(rendition),
                key: RenditionKey::new(format!("video/{rendition}")),
                source_tracks: Arc::from([TrackId(0)]),
                config: RenditionConfig {
                    timebase: Timebase::new(nz::u32!(1), nz::u32!(1)),
                    segment_target: NonZero::new(6).unwrap(),
                    chunk_target: chunked.then(|| NonZero::new(1).unwrap()),
                    segment_format: MediaSegmentFormat::Cmaf,
                },
                media: RenditionMedia::Video {
                    width: nz::u32!(1920),
                    height: nz::u32!(1080),
                    frame_rate: Some(FrameRate::new(nz::u32!(30), nz::u32!(1))),
                    video_range: None,
                },
                codecs: Arc::from("avc1.640028"),
                name: Arc::from(format!("Video {rendition}")),
                language: None,
                is_default: rendition == 0,
                declared_bandwidth: None,
            })
            .collect();
        let ids: Arc<[PackagingRenditionId]> = configs
            .iter()
            .map(|&(rendition, _)| PackagingRenditionId(rendition))
            .collect::<Vec<_>>()
            .into();
        Arc::new(
            PackagedPresentation::new(
                SystemTime::UNIX_EPOCH,
                &input,
                renditions,
                vec![RenditionGroup {
                    key: RenditionGroupKey::new("video"),
                    media_kind: MediaKind::Video,
                    renditions: ids,
                }],
                vec![PlayableCombination {
                    groups: Arc::from([RenditionGroupKey::new("video")]),
                }],
            )
            .expect("test packaged presentation is valid"),
        )
    }

    fn lease(store: &StreamStore, configs: &[(u32, bool)]) -> StreamLease {
        store
            .lease(stream(), packaged_presentation(configs))
            .expect("test publication fits")
    }

    fn keyed_presentation(
        local_id: u32,
        key: &str,
        chunked: bool,
        time_anchor: SystemTime,
    ) -> Arc<PackagedPresentation> {
        let mut packaged = packaged_presentation(&[(local_id, chunked)]);
        let presentation = Arc::make_mut(&mut packaged);
        presentation.time_anchor = time_anchor;
        Arc::make_mut(&mut presentation.renditions)[0].key = RenditionKey::new(key);
        packaged
    }

    fn initialization(rendition: u32, byte: u8) -> PackagedMedia {
        PackagedMedia::Initialization(InitializationSegment {
            rendition_id: PackagingRenditionId(rendition),
            version: u64::from(byte),
            payload: Payload::from(vec![byte]),
        })
    }

    fn chunk(
        rendition: u32,
        segment: u64,
        index: u32,
        start: i64,
        duration: u64,
        bytes: usize,
    ) -> PackagedMedia {
        PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(rendition),
            packaging_segment_id: PackagingSegmentId(segment),
            chunk_index: index,
            media_start: start,
            duration,
            independent: index == 0,
            payload: Payload::from(vec![index as u8; bytes]),
        })
    }

    fn completion(rendition: u32, segment: u64, start: i64, duration: u64) -> PackagedMedia {
        PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
            rendition_id: PackagingRenditionId(rendition),
            packaging_segment_id: PackagingSegmentId(segment),
            media_start: start,
            duration,
        })
    }

    fn direct(
        rendition: u32,
        segment: u64,
        start: i64,
        duration: u64,
        bytes: usize,
    ) -> PackagedMedia {
        PackagedMedia::Segment(PackagedSegment {
            rendition_id: PackagingRenditionId(rendition),
            packaging_segment_id: PackagingSegmentId(segment),
            media_start: start,
            duration,
            independent: true,
            payload: Payload::from(vec![segment as u8; bytes]),
        })
    }

    fn write(lease: &StreamLease, media: PackagedMedia) {
        assert_eq!(lease.write(media), Ok(true));
    }

    fn configure(lease: &StreamLease, rendition: u32, chunked: bool) {
        let catalog = lease.live().snapshot();
        let snapshot = catalog
            .renditions
            .iter()
            .find(|entry| entry.key == RenditionKey::new(format!("video/{rendition}")))
            .expect("rendition was configured by the descriptor");
        assert_eq!(
            snapshot
                .config
                .and_then(|config| config.chunk_target)
                .is_some(),
            chunked
        );
        write(lease, initialization(rendition, 1));
    }

    #[test]
    fn fractional_playlist_duration_policy_controls_the_visible_window() {
        let mut limits = limits();
        limits.retention.minimum_playlist_segments = 0;
        limits.retention.minimum_playlist_duration = DurationRule::MultipleOfTarget(
            TargetDurationMultiple::new(3, 2).expect("denominator is nonzero"),
        );
        let store = StreamStore::new(limits);
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);

        for id in 0..3 {
            write(&lease, direct(0, id, id as i64 * 6, 6, 1));
        }

        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert_eq!(snapshot.segments.len(), 2);
        assert_eq!(snapshot.segments[0].msn, Msn(1));
    }

    #[test]
    fn invalid_packaging_sequences_are_rejected_atomically() {
        let store = store();
        let lease = lease(&store, &[(0, true)]);
        assert_eq!(
            lease.write(chunk(9, 0, 0, 0, 1, 3)),
            Err(StoreWriteError::UnknownPackagingRendition {
                rendition_id: PackagingRenditionId(9)
            })
        );

        assert_eq!(
            lease.write(chunk(0, 0, 0, 0, 1, 3)),
            Err(StoreWriteError::InitializationMissing {
                rendition_id: RenditionId(0)
            })
        );
        write(&lease, initialization(0, 1));
        write(&lease, chunk(0, 0, 0, 0, 1, 3));
        let bytes = lease.live().retained_payload_bytes();
        assert_eq!(
            lease.write(chunk(0, 0, 2, 1, 1, 7)),
            Err(StoreWriteError::UnexpectedChunkIndex {
                rendition_id: RenditionId(0),
                expected: 1,
                found: 2,
            })
        );
        assert_eq!(
            lease.write(completion(0, 1, 0, 1)),
            Err(StoreWriteError::WrongSegmentCompleted {
                rendition_id: RenditionId(0),
                open: PackagingSegmentId(0),
                found: PackagingSegmentId(1),
            })
        );
        assert!(matches!(
            lease.write(direct(0, 1, 0, 6, 4)),
            Err(StoreWriteError::DirectSegmentDuringOpenSegment { .. })
        ));
        assert_eq!(
            lease.write(initialization(0, 2)),
            Err(StoreWriteError::InitializationDuringOpenSegment {
                rendition_id: RenditionId(0)
            })
        );
        assert_eq!(lease.live().retained_payload_bytes(), bytes);
        assert_eq!(
            lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .open_segment
                .as_ref()
                .unwrap()
                .parts
                .len(),
            1
        );

        write(&lease, completion(0, 0, 0, 1));
        assert_eq!(
            lease.write(chunk(0, 0, 0, 1, 1, 1)),
            Err(StoreWriteError::NonMonotonicSegmentId {
                rendition_id: RenditionId(0),
                previous: PackagingSegmentId(0),
                found: PackagingSegmentId(0),
            })
        );
        assert_eq!(
            lease.write(completion(0, 1, 1, 1)),
            Err(StoreWriteError::NoOpenSegment {
                rendition_id: RenditionId(0)
            })
        );
    }

    #[test]
    fn rendition_configuration_selects_chunked_or_segment_only_packaging() {
        let store = store();
        let lease = lease(&store, &[(0, false), (1, true)]);
        configure(&lease, 0, false);
        assert_eq!(
            lease.write(chunk(0, 0, 0, 0, 1, 1)),
            Err(StoreWriteError::ChunksDisabled {
                rendition_id: RenditionId(0)
            })
        );

        configure(&lease, 1, true);
        assert_eq!(
            lease.write(direct(1, 0, 0, 6, 1)),
            Err(StoreWriteError::DirectSegmentsDisabled {
                rendition_id: RenditionId(1)
            })
        );
        assert!(
            lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .segments
                .is_empty()
        );
        assert!(
            lease
                .live()
                .rendition(RenditionId(1))
                .unwrap()
                .segments
                .is_empty()
        );
    }

    #[test]
    fn completing_a_chunked_segment_reuses_the_original_payloads() {
        let store = store();
        let lease = lease(&store, &[(0, true)]);
        configure(&lease, 0, true);
        let original = Payload::from(vec![1, 2, 3, 4]);
        let pointer = original.as_bytes().as_ptr();
        write(
            &lease,
            PackagedMedia::Chunk(PackagedChunk {
                rendition_id: PackagingRenditionId(0),
                packaging_segment_id: PackagingSegmentId(0),
                chunk_index: 0,
                media_start: 0,
                duration: 6,
                independent: true,
                payload: original,
            }),
        );
        assert!(
            !lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .has_completed_segment(),
            "the first parent segment is still open"
        );
        write(&lease, completion(0, 0, 0, 6));

        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert!(snapshot.has_completed_segment());
        let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &snapshot.segments[0].kind
        else {
            panic!("segment should retain its chunks");
        };
        assert_eq!(parts[0].payload.as_bytes().as_ptr(), pointer);
        assert_eq!(
            snapshot.segments[0].kind,
            StoredSegmentKind::Media(SegmentBody::Chunked(Arc::clone(parts)))
        );
        assert_eq!(lease.live().retained_payload_bytes(), 5);
        assert_eq!(
            snapshot.bitrate,
            RenditionBitrateStatistics {
                peak_bits_per_second: Some(5),
                average_bits_per_second: Some(5),
                observed_segments: 1,
            }
        );
    }

    #[test]
    fn direct_segments_remain_contiguous() {
        let store = store();
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        write(&lease, direct(0, 0, 0, 6, 9));

        let segment = &lease.live().rendition(RenditionId(0)).unwrap().segments[0];
        assert!(matches!(
            &segment.kind,
            StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) if payload.len() == 9
        ));
        assert!(segment.parts.is_empty());
        assert_eq!(
            lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .bitrate
                .average_bits_per_second,
            Some(12)
        );
    }

    #[test]
    fn durable_part_ids_and_cursors_advance_independently() {
        let store = store();
        let lease = lease(&store, &[(0, true)]);
        configure(&lease, 0, true);
        assert_eq!(
            lease
                .live()
                .rendition_live_edge(RenditionId(0))
                .unwrap()
                .next_part_id,
            Some(PartId(1))
        );
        write(&lease, chunk(0, 0, 0, 0, 1, 1));
        assert_eq!(
            lease.live().rendition_live_edge(RenditionId(0)).unwrap(),
            RenditionLiveEdge {
                last_segment: None,
                last_part: Some((
                    PartCursor {
                        msn: Msn(0),
                        part_index: PartIndex(0)
                    },
                    PartId(1)
                )),
                next_part_id: Some(PartId(2)),
                ended: false,
                revision: 3,
            }
        );
        write(&lease, completion(0, 0, 0, 1));
        write(&lease, chunk(0, 1, 0, 1, 1, 1));

        let edge = lease.live().rendition_live_edge(RenditionId(0)).unwrap();
        assert_eq!(edge.last_segment, Some((Msn(0), SegmentId(1))));
        assert_eq!(
            edge.last_part,
            Some((
                PartCursor {
                    msn: Msn(1),
                    part_index: PartIndex(0)
                },
                PartId(2)
            ))
        );
        assert_eq!(edge.next_part_id, Some(PartId(3)));
    }

    #[test]
    fn takeover_consumes_an_observed_open_msn_as_a_gap() {
        let store = store();
        let first = lease(&store, &[(0, true)]);
        configure(&first, 0, true);
        write(&first, chunk(0, 0, 0, 0, 1, 1));

        let second = lease(&store, &[(0, true)]);
        assert_eq!(first.write(chunk(0, 0, 1, 1, 1, 1)), Ok(false));
        configure(&second, 0, true);
        write(&second, chunk(0, 0, 0, 0, 1, 1));

        let snapshot = second.live().rendition(RenditionId(0)).unwrap();
        assert!(matches!(snapshot.segments[0].kind, StoredSegmentKind::Gap));
        assert_eq!(snapshot.segments[0].msn, Msn(0));
        let open = snapshot.open_segment.as_ref().unwrap();
        assert_eq!(open.msn, Msn(1));
        assert_eq!(open.parts[0].id, PartId(2));
        assert_eq!(open.parts[0].cursor.part_index, PartIndex(0));
    }

    #[test]
    fn reconnect_matches_exact_keys_even_when_local_ids_change() {
        let store = store();
        let first = store
            .lease(
                stream(),
                keyed_presentation(0, "camera/main", false, SystemTime::UNIX_EPOCH),
            )
            .unwrap();
        write(&first, initialization(0, 1));
        write(&first, direct(0, 0, 0, 6, 1));

        let second_anchor = SystemTime::UNIX_EPOCH + Duration::from_secs(30);
        let second = store
            .lease(
                stream(),
                keyed_presentation(9, "camera/main", false, second_anchor),
            )
            .unwrap();
        write(&second, initialization(9, 2));
        write(&second, direct(9, 0, 0, 6, 1));

        let catalog = second.live().snapshot();
        assert_eq!(catalog.renditions.len(), 1);
        assert_eq!(catalog.renditions[0].rendition_id, RenditionId(0));
        assert_eq!(
            catalog.presentation.as_ref().unwrap().groups[0]
                .renditions
                .as_ref(),
            &[RenditionId(0)]
        );
        assert_eq!(catalog.publication_anchors.len(), 2);
        assert_eq!(catalog.publication_anchors[1].time_anchor, second_anchor);
        let media = catalog.renditions[0].snapshot();
        assert_eq!(media.segments.len(), 2);
        assert_eq!(media.segments[1].msn, Msn(1));
    }

    #[test]
    fn changed_topology_retires_old_renditions_instead_of_fuzzy_matching() {
        let store = store();
        let first = store
            .lease(
                stream(),
                keyed_presentation(0, "camera/main", false, SystemTime::UNIX_EPOCH),
            )
            .unwrap();
        write(&first, initialization(0, 1));
        write(&first, direct(0, 0, 0, 6, 1));

        let second = store
            .lease(
                stream(),
                keyed_presentation(0, "camera/replacement", false, SystemTime::UNIX_EPOCH),
            )
            .unwrap();
        write(&second, initialization(0, 1));

        let catalog = second.live().snapshot();
        assert_eq!(catalog.renditions.len(), 2);
        assert!(!catalog.renditions[0].active);
        assert!(catalog.renditions[0].snapshot().live_edge.ended);
        assert!(catalog.renditions[1].active);
        assert_eq!(catalog.renditions[1].rendition_id, RenditionId(1));
        assert_eq!(
            catalog.presentation.as_ref().unwrap().groups[0]
                .renditions
                .as_ref(),
            &[RenditionId(1)]
        );
        assert!(
            second
                .live()
                .segment(RenditionId(0), SegmentId(1))
                .is_some(),
            "retired playlist resources remain fetchable through normal retention"
        );

        let third = store
            .lease(
                stream(),
                keyed_presentation(5, "camera/main", false, SystemTime::UNIX_EPOCH),
            )
            .unwrap();
        let catalog = third.live().snapshot();
        assert_eq!(
            catalog.presentation.as_ref().unwrap().groups[0]
                .renditions
                .as_ref(),
            &[RenditionId(2)],
            "an ended playlist is not resurrected when its key later returns"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rendition_watchers_are_isolated_and_bitrate_updates_bump_the_catalog() {
        let store = store();
        let lease = lease(&store, &[(0, true), (1, true)]);
        configure(&lease, 0, true);
        configure(&lease, 1, true);
        let catalog_revision = lease.live().revision();
        let mut first = lease.live().subscribe_rendition(RenditionId(0)).unwrap();
        let mut sibling = lease.live().subscribe_rendition(RenditionId(1)).unwrap();
        first.borrow_and_update();
        sibling.borrow_and_update();

        write(&lease, chunk(0, 0, 0, 0, 1, 1));
        first.changed().await.unwrap();
        assert!(!sibling.has_changed().unwrap());
        assert_eq!(lease.live().revision(), catalog_revision);

        write(&lease, completion(0, 0, 0, 1));
        assert!(first.has_changed().unwrap());
        assert!(!sibling.has_changed().unwrap());
        assert!(
            lease.live().revision() > catalog_revision,
            "completed-segment bitrate statistics invalidate the multivariant projection"
        );
        let catalog = lease.live().snapshot();
        assert_eq!(
            catalog.renditions[0].bandwidth,
            catalog.renditions[0].snapshot().bitrate.advertised(),
            "one catalog revision captures the bitrate values it advertises"
        );
    }

    #[test]
    fn unchanged_advertised_bitrate_does_not_churn_the_catalog() {
        let store = store();
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        write(&lease, direct(0, 0, 0, 6, 6));
        let revision = lease.live().revision();

        write(&lease, direct(0, 1, 6, 6, 6));

        assert_eq!(
            lease.live().revision(),
            revision,
            "observation counts are diagnostic; only advertised rates invalidate the manifest"
        );
    }

    #[test]
    fn request_snapshots_are_cached_until_the_next_committed_change() {
        let store = store();
        let lease = lease(&store, &[(0, true), (1, true)]);
        configure(&lease, 0, true);
        configure(&lease, 1, true);

        let catalog = lease.live().snapshot();
        let same_catalog = lease.live().snapshot();
        assert!(Arc::ptr_eq(&catalog, &same_catalog));
        let sibling = catalog.renditions[1].snapshot();
        let first = catalog.renditions[0].snapshot();
        let same = lease.live().rendition(RenditionId(0)).unwrap();
        assert!(Arc::ptr_eq(&first, &same));

        write(&lease, chunk(0, 0, 0, 0, 1, 1));
        let same_catalog = lease.live().snapshot();
        assert!(
            Arc::ptr_eq(&catalog, &same_catalog),
            "ordinary chunks do not rebuild the stream catalog"
        );
        assert!(
            Arc::ptr_eq(&sibling, &same_catalog.renditions[1].snapshot()),
            "an unrelated rendition does not rebuild its cached snapshot"
        );
        let advanced = same_catalog.renditions[0].snapshot();
        assert!(!Arc::ptr_eq(&first, &advanced));
        assert!(first.open_segment.is_none());
        assert_eq!(
            advanced.open_segment.as_ref().map(|open| open.parts.len()),
            Some(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn part_tags_and_resources_have_distinct_retention_deadlines() {
        let store = store();
        let lease = lease(&store, &[(0, true)]);
        configure(&lease, 0, true);
        write(&lease, chunk(0, 0, 0, 0, 1, 1));
        write(&lease, completion(0, 0, 0, 1));
        let part_id = PartId(1);

        for id in 1..=4 {
            let start = 1 + (id as i64 - 1) * 6;
            write(&lease, chunk(0, id, 0, start, 6, 1));
            write(&lease, completion(0, id, start, 6));
        }
        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert!(
            snapshot.segments[0].parts.is_empty(),
            "the tag is hidden once it is over three targets behind"
        );
        assert!(lease.live().part(RenditionId(0), part_id).is_some());

        tokio::time::advance(Duration::from_secs(18)).await;
        assert!(lease.live().part(RenditionId(0), part_id).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn removed_segments_obey_their_availability_deadline() {
        let store = store();
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        for id in 0..7 {
            write(&lease, direct(0, id, id as i64 * 6, 6, 1));
            if id < 6 {
                tokio::time::advance(Duration::from_secs(6)).await;
            }
        }
        let first = SegmentId(1);
        assert!(lease.live().segment(RenditionId(0), first).is_some());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(lease.live().segment(RenditionId(0), first).is_none());
        assert_eq!(
            lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .segments
                .len(),
            6
        );
    }

    #[test]
    fn live_window_never_falls_below_three_target_durations() {
        let store = store();
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        for id in 0..19 {
            write(&lease, direct(0, id, id as i64, 1, 1));
        }

        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert_eq!(
            snapshot.segments.len(),
            18,
            "the six-segment count is only a floor when segments are shorter \
             than the target duration"
        );
        assert_eq!(snapshot.segments[0].msn, Msn(1));
    }

    #[tokio::test(start_paused = true)]
    async fn initializations_live_until_every_dependent_resource_expires() {
        let store = store();
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        write(&lease, direct(0, 0, 0, 6, 1));
        write(&lease, initialization(0, 2));
        for id in 1..7 {
            write(&lease, direct(0, id, id as i64 * 6, 6, 1));
        }

        assert_eq!(
            lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .initializations
                .len(),
            2
        );
        tokio::time::advance(Duration::from_secs(42)).await;
        store.maintain();
        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert_eq!(snapshot.initializations.len(), 1);
        assert_eq!(snapshot.initializations[0].version, 2);
    }

    #[test]
    fn payload_capacity_failure_does_not_mutate_the_open_segment() {
        let mut limits = limits();
        limits.retention.maximum_payload_bytes = 4;
        let store = StreamStore::new(limits);
        let lease = lease(&store, &[(0, true)]);
        configure(&lease, 0, true);
        write(&lease, chunk(0, 0, 0, 0, 1, 3));
        assert_eq!(lease.live().retained_payload_bytes(), 4);

        assert_eq!(
            lease.write(chunk(0, 0, 1, 1, 1, 1)),
            Err(StoreWriteError::PayloadCapacityExceeded {
                maximum: 4,
                additional: 1,
            })
        );
        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        let open = snapshot.open_segment.as_ref().unwrap();
        assert_eq!(open.parts.len(), 1);
        assert_eq!(lease.live().retained_payload_bytes(), 4);
    }

    #[test]
    fn object_capacity_failure_is_atomic_even_for_empty_payloads() {
        let mut limits = limits();
        limits.retention.maximum_parts = 1;
        let store = StreamStore::new(limits);
        let lease = lease(&store, &[(0, true)]);
        configure(&lease, 0, true);
        write(&lease, chunk(0, 0, 0, 0, 1, 0));

        assert_eq!(
            lease.write(chunk(0, 0, 1, 1, 1, 0)),
            Err(StoreWriteError::PartCapacityExceeded { maximum: 1 })
        );
        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert_eq!(snapshot.open_segment.as_ref().unwrap().parts.len(), 1);
        assert_eq!(snapshot.live_edge.next_part_id, Some(PartId(2)));
    }

    #[test]
    fn segment_count_capacity_failure_is_atomic() {
        let mut limits = limits();
        limits.retention.maximum_segments = 6;
        let store = StreamStore::new(limits);
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        for id in 0..6 {
            write(&lease, direct(0, id, id as i64 * 6, 6, 0));
        }

        assert_eq!(
            lease.write(direct(0, 6, 36, 6, 0)),
            Err(StoreWriteError::SegmentCapacityExceeded { maximum: 6 })
        );
        let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
        assert_eq!(snapshot.segments.len(), 6);
        assert_eq!(
            snapshot.live_edge.last_segment,
            Some((Msn(5), SegmentId(6)))
        );
    }

    #[test]
    fn ending_a_rendition_removes_its_preload_reservation() {
        let store = store();
        let lease = lease(&store, &[(0, true)]);
        configure(&lease, 0, true);
        write(&lease, chunk(0, 0, 0, 0, 1, 1));
        assert!(lease.end());

        let edge = lease.live().rendition_live_edge(RenditionId(0)).unwrap();
        assert!(edge.ended);
        assert_eq!(edge.next_part_id, None);
        assert_eq!(edge.last_segment, Some((Msn(0), SegmentId(1))));
        assert_eq!(
            lease
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .bitrate
                .observed_segments,
            0,
            "the synthesized gap is not a bitrate observation"
        );
    }

    #[test]
    fn peak_is_monotonic_and_average_uses_the_latest_media_hour() {
        let store = store();
        let lease = lease(&store, &[(0, false)]);
        configure(&lease, 0, false);
        for id in 0..600 {
            write(&lease, direct(0, id, id as i64 * 6, 6, 12));
        }
        for id in 600..1_200 {
            write(&lease, direct(0, id, id as i64 * 6, 6, 6));
        }

        let stats = lease.live().rendition(RenditionId(0)).unwrap().bitrate;
        assert_eq!(stats.peak_bits_per_second, Some(16));
        assert_eq!(stats.average_bits_per_second, Some(8));
        assert_eq!(stats.observed_segments, 1_200);
    }

    #[test]
    fn bitrate_peak_does_not_span_a_publication_discontinuity() {
        let store = store();
        let first = lease(&store, &[(0, false)]);
        configure(&first, 0, false);
        write(&first, direct(0, 0, 0, 2, 100));

        let second = lease(&store, &[(0, false)]);
        configure(&second, 0, false);
        write(&second, direct(0, 0, 0, 2, 100));
        assert_eq!(
            second
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .bitrate
                .peak_bits_per_second,
            None
        );

        write(&second, direct(0, 1, 2, 2, 100));
        assert_eq!(
            second
                .live()
                .rendition(RenditionId(0))
                .unwrap()
                .bitrate
                .peak_bits_per_second,
            Some(400)
        );
    }

    #[tokio::test]
    async fn lock_free_lookups_coexist_with_takeover_and_retirement_checks() {
        let store = Arc::new(store());
        let held = lease(&store, &[(0, true)]);
        let mut readers = Vec::new();
        for _ in 0..8 {
            let store = Arc::clone(&store);
            readers.push(tokio::spawn(async move {
                for _ in 0..1_000 {
                    assert!(store.get(&stream()).is_some());
                }
            }));
        }
        for _ in 0..32 {
            drop(lease(&store, &[(0, true)]));
            store.maintain();
        }
        for reader in readers {
            reader.await.unwrap();
        }
        assert!(store.get(&stream()).is_some());
        drop(held);
    }
}
