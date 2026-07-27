//! The slow-changing, request-facing view of one logical stream.
//!
//! Topology, lifecycle, and multivariant attributes replace a whole
//! [`StreamSnapshot`]; ordinary chunks replace only the [`RenditionSnapshot`]
//! behind one [`RenditionView`]. That split is why viewer count does not
//! multiply snapshot construction: a request loads and clones the latest
//! [`Arc`] and never rebuilds a vector.

use std::{fmt, sync::Arc, time::SystemTime};

use arc_swap::ArcSwap;

use crate::{
    domain::{MediaKind, RenditionId},
    mux::{PlayableCombination, RenditionConfig, RenditionGroupKey, RenditionKey, RenditionMedia},
};

use super::{PlaylistContract, RenditionBandwidth, RenditionSnapshot};

/// Atomically published media-playlist state for one rendition.
///
/// The stream catalog retains one stable handle per rendition. Ordinary chunks
/// replace only this handle's latest snapshot, avoiding reconstruction of
/// sibling renditions or the slow-changing stream catalog. The store retains
/// only the latest value; an older value lives solely while an in-flight
/// request still holds its [`Arc`].
pub struct RenditionView {
    rendition_id: RenditionId,
    latest: ArcSwap<RenditionSnapshot>,
}

impl RenditionView {
    pub fn new(snapshot: RenditionSnapshot) -> Self {
        Self {
            rendition_id: snapshot.rendition_id,
            latest: ArcSwap::from_pointee(snapshot),
        }
    }

    /// Returns the already-built latest snapshot without cloning its vectors.
    pub fn snapshot(&self) -> Arc<RenditionSnapshot> {
        self.latest.load_full()
    }

    pub fn publish(&self, snapshot: RenditionSnapshot) {
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
    /// This rendition's frozen playlist terms.
    ///
    /// Carried on the catalog entry as well as the media snapshot because
    /// presentation-wide values — HLS requires one identical
    /// `EXT-X-SERVER-CONTROL` across every playlist of a multivariant
    /// presentation — are derived across renditions, and deriving them should
    /// not mean loading every sibling's media snapshot.
    pub contract: PlaylistContract,
    pub media: RenditionMedia,
    pub codecs: Arc<str>,
    pub name: Arc<str>,
    pub language: Option<Arc<str>>,
    pub is_default: bool,
    pub declared_bandwidth: Option<u64>,
    pub bandwidth: RenditionBandwidth,
    pub view: Arc<RenditionView>,
}

impl RenditionCatalogEntry {
    pub fn snapshot(&self) -> Arc<RenditionSnapshot> {
        self.view.snapshot()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRenditionGroup {
    pub key: RenditionGroupKey,
    pub media_kind: MediaKind,
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
