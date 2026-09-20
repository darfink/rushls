//! Turning retained media into the playlists that describe it.
//!
//! This layer decides *which* tags a snapshot calls for and with what values;
//! [`manifest`](crate::delivery::hls::manifest) below it knows only how each
//! tag is spelled, and the store beneath that knows only what is retained.
//!
//! Everything here is a pure function of an immutable snapshot. No clock is
//! read, no lock is taken, and nothing is cached: a projection given the same
//! snapshot twice produces the same bytes, which is what makes rendering
//! cacheable by whoever holds the snapshot and what makes every rule below
//! testable without a running stream.
//!
//! Manifest names come from [`uri`](crate::delivery::hls::uri); its media names
//! are derived from the protocol-neutral namespace the HTTP application routes.
//!
//! | Module | Owns |
//! |---|---|
//! | [`timing`] | Values derived from the locked plan, not from arrivals |
//! | [`media`] | One rendition's media playlist |
//! | [`multivariant`] | The presentation a player chooses from |

use std::num::NonZeroU64;

use thiserror::Error;

use crate::{
    delivery::hls::{InitializationId, StreamSnapshot, manifest::ManifestWriteError},
    domain::RenditionId,
};

pub mod media;
pub mod multivariant;
pub mod timing;

#[cfg(test)]
mod tests;

pub use timing::DeliveryTimingPolicy;

/// Which Media Playlist form a client asked this origin to render.
///
/// `_HLS_skip=YES` and `_HLS_skip=v2` select a Playlist Delta Update; anything
/// else — including an absent or unknown value — is the full window. Unknown
/// values are not errors: they are forward-compatible protocol extensions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlaylistDelta {
    #[default]
    Full,
    /// Omit completed parents further than `CAN-SKIP-UNTIL` from the last one.
    Skip,
    /// The same skip, plus empty `RECENTLY-REMOVED-DATERANGES` (version 10).
    SkipV2,
}

impl PlaylistDelta {
    /// Present only on a `v2` delta, and empty until this origin tracks
    /// date-range identity. The attribute is what requires version 10.
    pub fn recently_removed_dateranges(self) -> Option<&'static str> {
        match self {
            Self::SkipV2 => Some(""),
            Self::Full | Self::Skip => None,
        }
    }
}

/// When a playlist restates the wall-clock time of its media.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProgramDateTimePolicy {
    /// Once per continuous range: at the playlist head and after each
    /// discontinuity. A client accumulates EXTINF from there, so this is
    /// sufficient, and it keeps a long window from repeating a timestamp on
    /// every line.
    #[default]
    AtDiscontinuities,
    /// On every segment. Costs bytes, but survives a client that resynchronises
    /// mid-playlist.
    EverySegment,
}

/// Everything a projection needs that is not in a snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistPolicy {
    /// Advertise and serve a sparse I-frame playlist for each CMAF video rendition.
    pub iframe_playlists: bool,
    pub program_date_time: ProgramDateTimePolicy,
    /// What to advertise for a rendition that has neither measured nor
    /// declared a bitrate.
    ///
    /// `BANDWIDTH` is required on every variant, so this is what makes a
    /// presentation servable from its first instant rather than after its first
    /// completed segment. It should be *generous*: a client that under-fetches
    /// because the origin lowballed an unknown rate stalls, while one that
    /// over-estimates merely starts conservatively and corrects within a
    /// segment. It is not a floor for measured values, which always win.
    pub assumed_bandwidth: NonZeroU64,
}

impl Default for PlaylistPolicy {
    fn default() -> Self {
        Self {
            iframe_playlists: true,
            program_date_time: ProgramDateTimePolicy::default(),
            // Deliberately high: unknown is not the same as small, and the cost
            // of guessing high is one conservative segment.
            assumed_bandwidth: nz::u64!(6_000_000),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProjectionError {
    #[error("a playlist tag could not be rendered: {0}")]
    Manifest(#[from] ManifestWriteError),
    #[error("{rendition_id} has no packaging configuration to project")]
    RenditionUnconfigured { rendition_id: RenditionId },
    #[error("{rendition_id} references initialization {initialization}, which is not retained")]
    InitializationMissing {
        rendition_id: RenditionId,
        initialization: InitializationId,
    },
    #[error("a resource cannot be named in the format serving it")]
    UnnameableResource,
}

/// The frozen contracts of every rendition currently in the topology.
///
/// The presentation-wide values — one `EXT-X-SERVER-CONTROL` shared by every
/// playlist — are derived from these, so both the multivariant projection and
/// each media playlist ask the same question of the same snapshot.
pub fn active_contracts(
    stream: &StreamSnapshot,
) -> impl Iterator<Item = crate::delivery::hls::PlaylistContract> + '_ {
    stream
        .renditions
        .iter()
        .filter(|entry| entry.active)
        .map(|entry| entry.contract)
}

/// The one `EXT-X-SERVER-CONTROL` this presentation's playlists must share.
pub fn presentation_server_control(
    stream: &StreamSnapshot,
    policy: DeliveryTimingPolicy,
) -> Option<crate::delivery::hls::manifest::ServerControl> {
    timing::server_control(active_contracts(stream), policy)
}
