//! Shared retained-byte accounting, with an optional manifest allowance.
//!
//! The byte store owns cached buffers, while protocol caches hold weak handles.
//! Eviction never calls back into a protocol cache or takes its render lock.
//! Responses already holding Bytes are in-flight overhead after eviction.

use bytes::Bytes;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

#[derive(Clone, Copy, Debug)]
pub enum ManifestClass {
    Index,
    Media,
}
impl ManifestClass {
    fn index(self) -> usize {
        match self {
            Self::Index => 0,
            Self::Media => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemoryBudget(Arc<Mutex<State>>);

#[derive(Debug)]
struct State {
    capacity: usize,
    manifest_capacity: usize,
    media: usize,
    manifests: usize,
    clock: u64,
    next_id: u64,
    epochs: [u64; 2],
    entries: HashMap<u64, Entry>,
}
#[derive(Debug)]
struct Entry {
    plain: Bytes,
    gzip: Bytes,
    class: ManifestClass,
    epoch: u64,
    touched: u64,
}
impl Entry {
    fn len(&self) -> usize {
        self.plain.len().saturating_add(self.gzip.len())
    }
}

/// Clones share one cache charge. Dropping the last handle removes the entry.
#[derive(Debug)]
pub struct ManifestHandle {
    budget: Weak<Mutex<State>>,
    id: u64,
}
impl ManifestHandle {
    pub fn get(&self) -> Option<(Bytes, Bytes)> {
        let budget = self.budget.upgrade()?;
        let mut state = budget.lock();
        state.clock = state.clock.saturating_add(1);
        let now = state.clock;
        let entry = state.entries.get_mut(&self.id)?;
        entry.touched = now;
        Some((entry.plain.clone(), entry.gzip.clone()))
    }
}
impl Drop for ManifestHandle {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.upgrade() {
            budget.lock().remove(self.id);
        }
    }
}
impl State {
    fn remove(&mut self, id: u64) {
        if let Some(entry) = self.entries.remove(&id) {
            self.manifests -= entry.len();
        }
    }
    fn reclaim(&mut self, additional: usize) {
        let available = self
            .capacity
            .saturating_sub(self.media)
            .min(self.manifest_capacity);
        while self
            .manifests
            .checked_add(additional)
            .is_none_or(|total| total > available)
        {
            let Some(id) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| {
                    // Obsolete epochs first, then approximate recency across all
                    // manifest forms. A hit costs one short accounting lock.
                    (
                        entry.epoch == self.epochs[entry.class.index()],
                        entry.touched,
                    )
                })
                .map(|(&id, _)| id)
            else {
                break;
            };
            self.remove(id);
        }
    }
}
impl MemoryBudget {
    pub fn new(capacity: usize) -> Self {
        Self::with_manifest_capacity(capacity, capacity)
    }
    /// Disk-backed streams cap manifests at their protected allowance. This
    /// prevents cache growth into the space needed for media spill hysteresis.
    pub fn with_manifest_capacity(capacity: usize, manifest_capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(State {
            capacity,
            manifest_capacity: manifest_capacity.min(capacity),
            media: 0,
            manifests: 0,
            clock: 0,
            next_id: 0,
            epochs: [0; 2],
            entries: HashMap::new(),
        })))
    }
    pub fn same_stream(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn usage(&self) -> (usize, usize) {
        let state = self.0.lock();
        (state.media, state.manifests)
    }
    /// Media may exceed capacity for the minimum live window. In that case
    /// no manifest can remain cached, and existing media overflow rules apply.
    pub fn set_media(&self, bytes: usize) {
        let mut state = self.0.lock();
        state.media = bytes;
        state.reclaim(0);
    }
    pub fn set_epoch(&self, class: ManifestClass, epoch: u64) {
        self.0.lock().epochs[class.index()] = epoch;
    }
    pub fn insert(
        &self,
        class: ManifestClass,
        epoch: u64,
        plain: Bytes,
        gzip: Bytes,
    ) -> Option<Arc<ManifestHandle>> {
        let bytes = plain.len().checked_add(gzip.len())?;
        let mut state = self.0.lock();
        // An oversized response must not flush useful entries just to fail.
        if bytes
            > state
                .capacity
                .saturating_sub(state.media)
                .min(state.manifest_capacity)
        {
            return None;
        }
        state.reclaim(bytes);
        let id = state.next_id.checked_add(1)?;
        state.next_id = id;
        state.clock = state.clock.saturating_add(1);
        let touched = state.clock;
        state.entries.insert(
            id,
            Entry {
                plain,
                gzip,
                class,
                epoch,
                touched,
            },
        );
        state.manifests += bytes;
        Some(Arc::new(ManifestHandle {
            budget: Arc::downgrade(&self.0),
            id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(budget: &MemoryBudget, epoch: u64) -> Arc<ManifestHandle> {
        budget
            .insert(
                ManifestClass::Media,
                epoch,
                Bytes::from_static(b"plain"),
                Bytes::from_static(b"gzip!"),
            )
            .expect("ten bytes fit")
    }

    #[test]
    fn manifest_allowance_cannot_consume_the_spill_margin() {
        let budget = MemoryBudget::with_manifest_capacity(160, 20);
        let first = insert(&budget, 0);
        let second = insert(&budget, 0);
        let third = insert(&budget, 0);
        assert!(first.get().is_none());
        assert_eq!(budget.usage(), (0, 20));
        budget.set_media(130);
        assert!(second.get().is_some());
        assert!(third.get().is_some());
        assert_eq!(budget.usage(), (130, 20));
    }

    #[test]
    fn stale_entries_precede_recent_valid_entries_and_hits_update_recency() {
        let budget = MemoryBudget::new(30);
        let stale = insert(&budget, 0);
        budget.set_epoch(ManifestClass::Media, 1);
        let cold = insert(&budget, 1);
        let hot = insert(&budget, 1);
        assert!(stale.get().is_some()); // Newest access, but obsolete.
        let replacement = insert(&budget, 1);
        assert!(stale.get().is_none());
        assert!(cold.get().is_some()); // Now hot is the least recent valid entry.
        let last = insert(&budget, 1);
        assert!(hot.get().is_none());
        assert!(cold.get().is_some());
        assert!(replacement.get().is_some());
        assert!(last.get().is_some());
        assert_eq!(budget.usage(), (0, 30));
    }

    #[test]
    fn media_reclaims_cache_and_in_flight_bytes_survive() {
        let budget = MemoryBudget::new(20);
        let first = insert(&budget, 0);
        let response = first.get().expect("cached bytes");
        let second = insert(&budget, 0);
        budget.set_media(15);
        assert_eq!(budget.usage(), (15, 0));
        assert!(first.get().is_none());
        assert!(second.get().is_none());
        assert_eq!(response.0, "plain");
        budget.set_media(21); // The minimum live-window exception.
        assert!(
            budget
                .insert(
                    ManifestClass::Media,
                    0,
                    Bytes::from_static(b"x"),
                    Bytes::new()
                )
                .is_none()
        );
        budget.set_media(0);
        let recovered = insert(&budget, 0);
        assert!(recovered.get().is_some());
    }

    #[test]
    fn handle_clones_are_charged_once_and_oversized_responses_do_not_flush_cache() {
        let budget = MemoryBudget::new(10);
        let first = insert(&budget, 0);
        let clone = first.clone();
        assert!(
            budget
                .insert(
                    ManifestClass::Index,
                    0,
                    Bytes::from(vec![0; 11]),
                    Bytes::new()
                )
                .is_none()
        );
        drop(first);
        assert_eq!(budget.usage(), (0, 10));
        drop(clone);
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn concurrent_insertions_and_media_growth_keep_one_budget() {
        let budget = MemoryBudget::new(100);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let budget = &budget;
                scope.spawn(move || {
                    let mut held = Vec::new();
                    for iteration in 0..100 {
                        budget.set_media(iteration % 100);
                        if let Some(handle) = budget.insert(
                            ManifestClass::Media,
                            0,
                            Bytes::from_static(b"12345"),
                            Bytes::from_static(b"67890"),
                        ) {
                            held.push(handle);
                        }
                        let (media, manifests) = budget.usage();
                        assert!(media + manifests <= 100);
                    }
                });
            }
        });
        assert_eq!(budget.usage().1, 0);
    }
}
