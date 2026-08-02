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
}

counters! {
    OriginCounters => OriginSnapshot {
        media_served: u64 = Counter(
            "rushls_media_responses_served_total",
            "Media responses served to viewers."
        ),
        bytes_served: u64 = Counter(
            "rushls_bytes_served_total",
            "Media bytes served to viewers."
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
    pub fn media_served(&self, bytes: u64) {
        add(&self.counters.media_served, 1);
        add(&self.counters.bytes_served, bytes);
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
        playlists_served: u64 = Counter(
            "rushls_hls_playlists_served_total",
            "HLS playlist responses served to viewers."
        ),
        /// Playlists actually projected, as opposed to reused from a cache.
        playlists_rendered: u64 = Counter(
            "rushls_hls_playlists_rendered_total",
            "HLS playlists projected instead of reused from the render cache."
        ),
        blocking_reloads: u64 = Counter(
            "rushls_hls_blocking_reloads_total",
            "HLS blocking playlist reloads started."
        ),
        /// Blocking reloads that hit their deadline without the media arriving.
        blocking_reloads_expired: u64 = Counter(
            "rushls_hls_blocking_reloads_expired_total",
            "HLS blocking playlist reloads that expired before media arrived."
        ),
    }
}

impl HlsMeters {
    pub fn playlist_served(&self, rendered: bool) {
        add(&self.counters.playlists_served, 1);
        if rendered {
            add(&self.counters.playlists_rendered, 1);
        }
    }

    pub fn blocking_reload_started(&self) {
        add(&self.counters.blocking_reloads, 1);
    }

    pub fn blocking_reload_expired(&self) {
        add(&self.counters.blocking_reloads_expired, 1);
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

        meters.playlist_served(true);
        meters.playlist_served(false);
        meters.playlist_served(false);

        let snapshot = meters.snapshot();
        assert_eq!(snapshot.playlists_served, 3);
        assert_eq!(
            snapshot.playlists_rendered, 1,
            "two of the three were answered from the render cache"
        );
    }

    #[test]
    fn expired_waits_are_counted_apart_from_the_waits_themselves() {
        let meters = HlsMeters::default();

        meters.blocking_reload_started();
        meters.blocking_reload_started();
        meters.blocking_reload_expired();

        let snapshot = meters.snapshot();
        assert_eq!(
            (snapshot.blocking_reloads, snapshot.blocking_reloads_expired),
            (2, 1),
            "waiting is the protocol working; expiring is the origin falling \
             behind the cadence it advertised"
        );
    }
}
