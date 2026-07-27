//! Reusing a rendered playlist across the viewers who would get the same one.
//!
//! Projection is a pure function of immutable snapshots, so two requests
//! arriving between two publications must produce identical bytes. Rendering
//! them separately is pure waste — and at viewer counts where it matters, it is
//! the largest single cost on the request path, because a playlist is rebuilt
//! far more often than media is published.
//!
//! # What the key has to cover
//!
//! The subtle part is that a media playlist is **not** a function of its own
//! rendition alone. `EXT-X-RENDITION-REPORT` republishes every sibling's live
//! edge, so an audio part advancing changes the correct bytes of the video
//! playlist. A key of "catalog revision plus my own edge revision" would serve
//! a stale report indefinitely to a rendition that had itself gone quiet — and
//! a stale report is worse than none, because a client switching renditions
//! acts on it.
//!
//! The key is therefore the catalog revision, this rendition's edge revision,
//! and every sibling's edge revision.

use std::sync::Arc;

use arc_swap::ArcSwap;
use bytes::Bytes;
use parking_lot::Mutex;

use crate::domain::RenditionId;

use super::StreamSnapshot;

/// Identifies exactly the inputs a rendered media playlist depends on.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistKey {
    /// Changes on topology, lifecycle, and advertised-metadata replacement.
    catalog_revision: u64,
    /// Every active rendition's edge revision, in catalog order, including
    /// this playlist's own.
    edges: Vec<u64>,
}

impl PlaylistKey {
    /// Reads the inputs the multivariant playlist depends on.
    ///
    /// Bitrate and topology updates already advance the catalog revision.
    /// Including media edges here would rebuild the multivariant playlist for
    /// every part even though none of its rendered attributes changed.
    pub fn multivariant(stream: &StreamSnapshot) -> Self {
        Self {
            catalog_revision: stream.revision,
            edges: Vec::new(),
        }
    }

    /// Reads the current key for one media playlist of a stream.
    ///
    /// Sibling snapshots are loaded here rather than inside the projection so
    /// that the key and the render see the same instants: the same handles are
    /// then passed on to be rendered.
    pub fn media(stream: &StreamSnapshot) -> Self {
        Self {
            catalog_revision: stream.revision,
            edges: stream
                .renditions
                .iter()
                .map(|entry| entry.snapshot().live_edge.revision)
                .collect(),
        }
    }
}

/// Playlist bytes, and whether producing them cost a render.
///
/// Reported so the caller can meter cache effectiveness without the cache
/// having to know what a meter is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rendered {
    pub bytes: Bytes,
    pub freshly_rendered: bool,
}

/// A rendered playlist and the inputs it was rendered from.
#[derive(Clone, Debug, Default)]
struct Cached {
    key: PlaylistKey,
    rendered: Bytes,
}

/// One rendition's cached playlist text.
///
/// The render itself is serialised by a mutex while reads are lock-free. That
/// asymmetry is the point: a thousand viewers arriving at once should produce
/// one render, not a thousand, and the one that does the work should not block
/// the readers who could already have used the previous value.
#[derive(Debug, Default)]
pub struct PlaylistCache {
    latest: ArcSwap<Option<Cached>>,
    rendering: Mutex<()>,
}

impl PlaylistCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached text if it was rendered from exactly `key`.
    pub fn get(&self, key: &PlaylistKey) -> Option<Bytes> {
        self.latest
            .load()
            .as_ref()
            .as_ref()
            .filter(|cached| &cached.key == key)
            .map(|cached| cached.rendered.clone())
    }

    /// Returns the cached text, rendering it with `render` if it is stale.
    ///
    /// `render` may run more than once across threads only if the key changes
    /// underneath; for one key it runs once and the rest wait, which is the
    /// behaviour that matters when a publication wakes every blocked viewer at
    /// the same instant.
    pub fn get_or_render<E>(
        &self,
        key: PlaylistKey,
        render: impl FnOnce() -> Result<String, E>,
    ) -> Result<Rendered, E> {
        if let Some(cached) = self.get(&key) {
            return Ok(Rendered {
                bytes: cached,
                freshly_rendered: false,
            });
        }
        let _rendering = self.rendering.lock();
        // Re-checked under the lock: whoever held it may have rendered exactly
        // what this caller wanted while it waited.
        if let Some(cached) = self.get(&key) {
            return Ok(Rendered {
                bytes: cached,
                freshly_rendered: false,
            });
        }
        let bytes = Bytes::from(render()?.into_bytes());
        self.latest.store(Arc::new(Some(Cached {
            key,
            rendered: bytes.clone(),
        })));
        Ok(Rendered {
            bytes,
            freshly_rendered: true,
        })
    }
}

/// Per-rendition playlist caches for one stream.
#[derive(Debug, Default)]
pub struct StreamPlaylistCache {
    multivariant: PlaylistCache,
    renditions: Mutex<Vec<(RenditionId, Arc<PlaylistCache>)>>,
}

impl StreamPlaylistCache {
    pub fn multivariant(&self) -> &PlaylistCache {
        &self.multivariant
    }

    pub fn rendition(&self, rendition: RenditionId) -> Arc<PlaylistCache> {
        let mut renditions = self.renditions.lock();
        if let Some((_, cache)) = renditions.iter().find(|(id, _)| *id == rendition) {
            return Arc::clone(cache);
        }
        let cache = Arc::new(PlaylistCache::new());
        renditions.push((rendition, Arc::clone(&cache)));
        cache
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        delivery::hls::{
            StreamStore,
            fixtures::{audio, chunk, initialization, lease, video, write, write_segment},
        },
        domain::RenditionId,
    };

    use super::*;

    #[test]
    fn one_key_renders_once_however_many_viewers_ask() {
        let cache = PlaylistCache::new();
        let key = PlaylistKey::default();
        let mut renders = 0;

        for _ in 0..3 {
            let rendered = cache
                .get_or_render::<()>(key.clone(), || {
                    renders += 1;
                    Ok("#EXTM3U\n".to_owned())
                })
                .expect("rendering succeeds");
            assert_eq!(rendered.bytes.as_ref(), b"#EXTM3U\n");
        }

        assert_eq!(renders, 1);
    }

    #[test]
    fn a_siblings_advance_invalidates_a_playlist_that_reports_it() {
        let store = StreamStore::default();
        let lease = lease(&store, vec![video(0), audio(1)]);
        for local in [0, 1] {
            write(&lease, initialization(local, 1));
            write_segment(&lease, local, 0, 0);
        }

        let before = PlaylistKey::media(&lease.live().snapshot());

        // Only the *audio* rendition advances. The video playlist's own edge is
        // untouched, but its EXT-X-RENDITION-REPORT for audio is now wrong.
        write(&lease, chunk(1, 1, 0, 6));
        let after = PlaylistKey::media(&lease.live().snapshot());

        assert_ne!(
            before, after,
            "a key covering only the requested rendition would serve a stale \
             rendition report for as long as this rendition stayed quiet"
        );

        let cache = PlaylistCache::new();
        cache
            .get_or_render::<()>(before.clone(), || Ok("stale".to_owned()))
            .expect("renders");
        assert!(cache.get(&after).is_none());
        assert_eq!(
            cache.get(&before).as_deref(),
            Some(&b"stale"[..]),
            "the previous value is still identifiable as what it was"
        );
    }

    #[test]
    fn an_unchanged_stream_keeps_serving_the_same_bytes() {
        let store = StreamStore::default();
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let key = PlaylistKey::media(&lease.live().snapshot());
        let cache = PlaylistCache::new();
        let first = cache
            .get_or_render::<()>(key.clone(), || Ok("#EXTM3U\n".to_owned()))
            .expect("renders");
        let second = cache
            .get_or_render::<()>(PlaylistKey::media(&lease.live().snapshot()), || {
                panic!("nothing changed, so nothing should be re-rendered")
            })
            .expect("reuses");

        assert!(
            !second.freshly_rendered,
            "an unchanged stream is answered from the cache"
        );
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn a_part_does_not_invalidate_an_unchanged_multivariant_playlist() {
        let store = StreamStore::default();
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let before = lease.live().snapshot();
        let multivariant_before = PlaylistKey::multivariant(&before);
        let media_before = PlaylistKey::media(&before);

        write(&lease, chunk(0, 1, 0, 6));
        let after = lease.live().snapshot();

        assert_eq!(
            multivariant_before,
            PlaylistKey::multivariant(&after),
            "the catalog did not change, so the presentation did not change"
        );
        assert_ne!(
            media_before,
            PlaylistKey::media(&after),
            "the media playlist gained a part and a new live edge"
        );
    }

    #[test]
    fn each_rendition_gets_its_own_cache_and_keeps_it() {
        let caches = StreamPlaylistCache::default();

        let first = caches.rendition(RenditionId(0));
        let again = caches.rendition(RenditionId(0));
        let other = caches.rendition(RenditionId(1));

        assert!(Arc::ptr_eq(&first, &again));
        assert!(!Arc::ptr_eq(&first, &other));
    }
}
