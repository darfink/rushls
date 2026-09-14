//! What the protocol-neutral request path is doing, counted.
//!
//! Named for the origin rather than for delivery because
//! [`DeliveryMeters`](super::DeliveryMeters) already counts the *publishing*
//! side — media made available to viewers. These count the serving side, and
//! the two fail independently: a stream can be publishing perfectly while every
//! viewer times out behind a projection fault.
//!
//! Manifest projection and blocking-reload counters belong to their protocol
//! adapter; [`HlsMeters`] holds those separately below.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use super::counters::counters;

#[derive(Clone, Debug, Default)]
pub struct OriginMeters {
    counters: Arc<OriginCounters>,
    pub operations: super::OperationMeters,
}

counters! {
    OriginCounters => OriginSnapshot {
        media_resolved: u64 = Counter(
            "rushls_media_resolved_total",
            "Media objects resolved before HTTP response processing."
        ),
        bytes_resolved: u64 = Counter(
            "rushls_media_resolved_bytes_total",
            "Unencoded media bytes resolved before HTTP range and conditional processing."
        ),
        requests_rejected: u64 = Counter(
            "rushls_origin_requests_rejected_total",
            "Origin requests rejected as invalid or unsatisfiable."
        ),
        requests_not_found: u64 = Counter(
            "rushls_origin_requests_not_found_total",
            "Origin requests for streams or resources that were not found."
        ),
    }
}

impl OriginMeters {
    pub fn media_resolved(&self, bytes: u64) {
        add(&self.counters.media_resolved, 1);
        add(&self.counters.bytes_resolved, bytes);
    }

    pub fn request_rejected(&self) {
        add(&self.counters.requests_rejected, 1);
    }

    pub fn request_not_found(&self) {
        add(&self.counters.requests_not_found, 1);
    }

    pub fn snapshot(&self) -> OriginSnapshot {
        self.counters.snapshot()
    }
}

#[derive(Clone, Debug, Default)]
pub struct HlsMeters {
    counters: Arc<HlsCounters>,
}

counters! {
    HlsCounters => HlsSnapshot {
        playlists_resolved: u64 = Counter(
            "rushls_hls_playlists_resolved_total",
            "HLS playlists resolved before HTTP conditional response processing."
        ),
        /// Playlists actually projected, as opposed to reused from a cache.
        playlist_projections: u64 = Counter(
            "rushls_hls_playlist_projections_total",
            "HLS playlists projected instead of reused from the render cache."
        ),

    }
}

impl HlsMeters {
    pub fn playlist_resolved(&self, rendered: bool) {
        add(&self.counters.playlists_resolved, 1);
        if rendered {
            add(&self.counters.playlist_projections, 1);
        }
    }

    pub fn snapshot(&self) -> HlsSnapshot {
        self.counters.snapshot()
    }
}

fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hits_are_distinguishable_from_renders() {
        let meters = HlsMeters::default();

        meters.playlist_resolved(true);
        meters.playlist_resolved(false);
        meters.playlist_resolved(false);

        let snapshot = meters.snapshot();
        assert_eq!(snapshot.playlists_resolved, 3);
        assert_eq!(
            snapshot.playlist_projections, 1,
            "two of the three were answered from the render cache"
        );
    }
}
