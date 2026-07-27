//! What the request path is doing, counted.
//!
//! Named for the origin rather than for delivery because
//! [`DeliveryMeters`](super::DeliveryMeters) already counts the *publishing*
//! side — media made available to viewers. These count the serving side, and
//! the two fail independently: a stream can be publishing perfectly while every
//! viewer times out behind a projection fault.
//!
//! The two that matter most for diagnosis are the blocking-reload outcomes.
//! Waits that are satisfied are the protocol working as designed; waits that
//! expire mean the origin is not keeping up with the cadence it advertised, and
//! that is invisible in any ingest metric.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug, Default)]
pub struct OriginMeters {
    counters: Arc<OriginCounters>,
}

#[derive(Debug, Default)]
struct OriginCounters {
    playlists_served: AtomicU64,
    playlists_rendered: AtomicU64,
    media_served: AtomicU64,
    bytes_served: AtomicU64,
    blocking_reloads: AtomicU64,
    blocking_reloads_expired: AtomicU64,
    requests_rejected: AtomicU64,
    requests_not_found: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OriginSnapshot {
    pub playlists_served: u64,
    /// Playlists actually projected, as opposed to reused from a cache.
    ///
    /// The gap between this and `playlists_served` is what the render cache is
    /// worth; if they track each other, viewers are arriving between
    /// publications and the cache is not earning its keep.
    pub playlists_rendered: u64,
    pub media_served: u64,
    pub bytes_served: u64,
    pub blocking_reloads: u64,
    /// Blocking reloads that hit their deadline without the media arriving.
    pub blocking_reloads_expired: u64,
    pub requests_rejected: u64,
    pub requests_not_found: u64,
}

impl OriginMeters {
    pub fn playlist_served(&self, rendered: bool) {
        add(&self.counters.playlists_served, 1);
        if rendered {
            add(&self.counters.playlists_rendered, 1);
        }
    }

    pub fn media_served(&self, bytes: u64) {
        add(&self.counters.media_served, 1);
        add(&self.counters.bytes_served, bytes);
    }

    pub fn blocking_reload_started(&self) {
        add(&self.counters.blocking_reloads, 1);
    }

    pub fn blocking_reload_expired(&self) {
        add(&self.counters.blocking_reloads_expired, 1);
    }

    pub fn request_rejected(&self) {
        add(&self.counters.requests_rejected, 1);
    }

    pub fn request_not_found(&self) {
        add(&self.counters.requests_not_found, 1);
    }

    pub fn snapshot(&self) -> OriginSnapshot {
        OriginSnapshot {
            playlists_served: read(&self.counters.playlists_served),
            playlists_rendered: read(&self.counters.playlists_rendered),
            media_served: read(&self.counters.media_served),
            bytes_served: read(&self.counters.bytes_served),
            blocking_reloads: read(&self.counters.blocking_reloads),
            blocking_reloads_expired: read(&self.counters.blocking_reloads_expired),
            requests_rejected: read(&self.counters.requests_rejected),
            requests_not_found: read(&self.counters.requests_not_found),
        }
    }
}

fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hits_are_distinguishable_from_renders() {
        let meters = OriginMeters::default();

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
        let meters = OriginMeters::default();

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
