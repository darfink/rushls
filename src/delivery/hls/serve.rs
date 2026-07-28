//! Answering one delivery request, including the ones that have to wait.
//!
//! This is where a request stops being a path and becomes bytes: resources are
//! resolved against the store, playlists are projected, and the requests that
//! name media which does not exist yet are held until it does. It is
//! deliberately free of HTTP types — status codes, headers, and bodies are the
//! server layer's vocabulary — so every rule below is testable against a paused
//! clock with no socket in sight.
//!
//! Waiting is the substance of low-latency delivery, and all of it funnels
//! through one predicate over a rendition's live edge. A request either names
//! media the edge has already reached, in which case it is answered
//! immediately, or it names media just beyond the edge, in which case it parks
//! on that rendition's watch channel until the edge arrives or the deadline
//! passes.

use std::{collections::HashMap, sync::Arc, time::Duration};

use bytes::Bytes;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::time::timeout;

use crate::{
    delivery::hls::{
        LiveStream, Msn, PartId, PartIndex, RenditionCatalogEntry, RenditionLiveEdge,
        RenditionSnapshot, SegmentBody, StoredSegment, StoredSegmentKind, StreamSnapshot,
        StreamStore,
        cache::{PlaylistKey, StreamPlaylistCache},
        project::{
            self, DeliveryTimingPolicy, PlaylistPolicy, ProjectionError, media::media_playlist,
            multivariant::multivariant_playlist, timing::blocking_reload_deadline,
        },
        uri::{PLAYLIST_CONTENT_TYPE, Resource, UriBase},
    },
    domain::{Payload, RenditionId, StreamId},
    observe::OriginMeters,
};

const ADVANCE_SEGMENT_LIMIT: u64 = 2;
const ADVANCE_PART_WINDOW: Duration = Duration::from_secs(3);

/// When a media playlist is considered fit to serve.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlaylistReadiness {
    /// Hold the first request until a segment has completed.
    ///
    /// A playlist naming only the parts of a still-open segment is valid HLS,
    /// but enough players mishandle one that arriving late is better than
    /// arriving unplayable. Costs a segment of startup latency on the very
    /// first request only.
    #[default]
    CompletedSegment,
    /// Serve as soon as the playlist names any media at all, parts included.
    AnyMedia,
}

/// Everything the request path needs that is not in the store.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeliveryConfig {
    pub playlist: PlaylistPolicy,
    pub timing: DeliveryTimingPolicy,
    pub readiness: PlaylistReadiness,
    /// Where the names playlists emit are rooted. Relative by default.
    pub uri_base: UriBase,
}

/// A blocking playlist reload: "do not answer until you have this".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockingReload {
    pub msn: Msn,
    pub part: Option<PartIndex>,
}

impl BlockingReload {
    /// Builds a directive from the query parameters that carried it.
    ///
    /// A part index without a media sequence number names no position at all,
    /// so it is refused rather than interpreted as "the current segment" —
    /// which would change meaning between the request being sent and it being
    /// read.
    pub fn from_directives(
        msn: Option<u64>,
        part: Option<u32>,
    ) -> Result<Option<Self>, DeliveryError> {
        match (msn, part) {
            (None, None) => Ok(None),
            (None, Some(_)) => Err(DeliveryError::InvalidDirective(
                "_HLS_part requires _HLS_msn",
            )),
            (Some(msn), part) => Ok(Some(Self {
                msn: Msn(msn),
                part: part.map(PartIndex),
            })),
        }
    }
}

/// What a client asked this origin for.
///
/// A [`Resource`] and, where one applies, the position the client refuses to be
/// answered before. The resource already carries what its name claimed —
/// a playlist's media kind, a media resource's packaging — and delivery checks
/// each claim against the rendition that answers it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub resource: Resource,
    /// Blocking is a playlist-reload directive. HLS attaches it to nothing
    /// else, so one arriving on a media request is ignored rather than refused:
    /// a client sending it has not asked for anything impossible.
    pub blocking: Option<BlockingReload>,
}

impl Request {
    pub fn new(resource: Resource, blocking: Option<BlockingReload>) -> Self {
        Self { resource, blocking }
    }
}

/// How long a response may be reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Caching {
    /// The resource is named by a durable identity and its bytes will never
    /// change, so it may be held until it is evicted from the cache.
    Immutable,
    /// The resource describes a moving live edge and is stale the moment it is
    /// written.
    Live,
}

/// Bytes to send, in the buffers they were stored in.
///
/// A completed chunked segment is retained as the parts that composed it and is
/// never reassembled: copying several megabytes per request to produce one
/// contiguous buffer would undo the zero-copy path the whole pipeline is built
/// on. Callers write the frames in order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaBody {
    frames: Vec<Payload>,
    length: u64,
}

impl MediaBody {
    pub fn single(payload: Payload) -> Self {
        let length = payload.len() as u64;
        Self {
            frames: vec![payload],
            length,
        }
    }

    fn from_frames(frames: Vec<Payload>) -> Self {
        let length = frames.iter().map(|frame| frame.len() as u64).sum();
        Self { frames, length }
    }

    fn from_segment(segment: &StoredSegment) -> Self {
        match &segment.kind {
            StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) => {
                Self::single(payload.clone())
            }
            StoredSegmentKind::Media(SegmentBody::Chunked(parts)) => {
                Self::from_frames(parts.iter().map(|part| part.payload.clone()).collect())
            }
            // A gap has no bytes by construction; it exists to keep a media
            // sequence number from vanishing, not to be fetched.
            StoredSegmentKind::Gap => Self::default(),
        }
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn frames(&self) -> &[Payload] {
        &self.frames
    }

    /// Clips to a byte range, splitting frames where the range falls inside
    /// one.
    ///
    /// `end` is inclusive, as an HTTP byte range is. Slicing a [`Payload`] is a
    /// refcount operation, so a range spanning several stored parts still
    /// copies nothing.
    pub fn range(&self, start: u64, end: u64) -> Self {
        let mut frames = Vec::new();
        let mut position = 0_u64;
        for frame in &self.frames {
            let length = frame.len() as u64;
            let frame_end = position + length;
            if frame_end > start && position <= end {
                let from = start.saturating_sub(position).min(length);
                let to = (end + 1 - position).min(length);
                frames.push(Payload::from_bytes(
                    frame.bytes().slice(from as usize..to as usize),
                ));
            }
            position = frame_end;
        }
        Self::from_frames(frames)
    }
}

/// What the origin produced for one request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Body {
    /// Already-encoded playlist text.
    ///
    /// Bytes rather than a `String` because a playlist is commonly served from
    /// the render cache, and handing out a refcount is the point of caching it.
    Playlist(Bytes),
    Media(MediaBody),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    pub body: Body,
    pub content_type: &'static str,
    pub caching: Caching,
}

impl Response {
    /// Media named by a durable identity, whose bytes can never change.
    ///
    /// The media type comes from the name that asked for it, so what a resource
    /// is called and what it is served as cannot disagree.
    fn immutable(body: MediaBody, resource: &Resource) -> Result<Self, DeliveryError> {
        Ok(Self {
            body: Body::Media(body),
            content_type: resource
                .content_type()
                .ok_or(DeliveryError::UnknownResource)?,
            caching: Caching::Immutable,
        })
    }

    /// A playlist, which describes a live edge and is stale as it is written.
    fn playlist(bytes: Bytes) -> Self {
        Self {
            body: Body::Playlist(bytes),
            content_type: PLAYLIST_CONTENT_TYPE,
            caching: Caching::Live,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DeliveryError {
    #[error("no such stream")]
    UnknownStream,
    #[error("no such rendition")]
    UnknownRendition,
    #[error("no such resource, or it is no longer available")]
    UnknownResource,
    #[error("the request is not answerable: {0}")]
    InvalidDirective(&'static str),
    #[error("the requested media did not arrive within the deadline")]
    Unsatisfied,
    #[error("the playlist could not be projected")]
    Projection,
}

impl From<ProjectionError> for DeliveryError {
    fn from(_: ProjectionError) -> Self {
        Self::Projection
    }
}

/// The set of streams this origin serves.
#[derive(Clone, Debug)]
pub struct Origin {
    store: StreamStore,
    config: DeliveryConfig,
    meters: OriginMeters,
    /// Rendered playlists, per stream.
    ///
    /// Held here rather than in the store because a rendered playlist is a
    /// projection concern and the store is deliberately ignorant of playlists.
    /// Entries outlive their stream until [`Self::prune`] runs, which is the
    /// same maintenance tick that retires streams.
    caches: Arc<Mutex<HashMap<StreamId, Arc<StreamPlaylistCache>>>>,
}

impl Origin {
    pub fn new(store: StreamStore, config: DeliveryConfig) -> Self {
        Self::with_meters(store, config, OriginMeters::default())
    }

    pub fn with_meters(store: StreamStore, config: DeliveryConfig, meters: OriginMeters) -> Self {
        Self {
            store,
            config,
            meters,
            caches: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn store(&self) -> &StreamStore {
        &self.store
    }

    pub fn config(&self) -> &DeliveryConfig {
        &self.config
    }

    pub fn meters(&self) -> &OriginMeters {
        &self.meters
    }

    /// Drops cached playlists for streams the store no longer holds.
    ///
    /// Belongs on the same maintenance tick as
    /// [`StreamStore::maintain`](crate::delivery::hls::StreamStore::maintain):
    /// a cache entry is small, but one per stream retained forever is a leak
    /// with the lifetime of the process.
    pub fn prune(&self) -> usize {
        let live: Vec<StreamId> = self.store.streams();
        let mut caches = self.caches.lock();
        let before = caches.len();
        caches.retain(|stream, _| live.contains(stream));
        before - caches.len()
    }

    fn cache_for(&self, stream: &StreamId) -> Arc<StreamPlaylistCache> {
        let mut caches = self.caches.lock();
        Arc::clone(
            caches
                .entry(stream.clone())
                .or_insert_with(|| Arc::new(StreamPlaylistCache::default())),
        )
    }

    pub async fn serve(
        &self,
        stream: &StreamId,
        request: Request,
    ) -> Result<Response, DeliveryError> {
        let outcome = self.dispatch(stream, request).await;
        match &outcome {
            Ok(Response {
                body: Body::Media(media),
                ..
            }) => self.meters.media_served(media.len()),
            Err(DeliveryError::InvalidDirective(_)) => self.meters.request_rejected(),
            Err(
                DeliveryError::UnknownStream
                | DeliveryError::UnknownRendition
                | DeliveryError::UnknownResource,
            ) => self.meters.request_not_found(),
            Err(DeliveryError::Unsatisfied) => self.meters.blocking_reload_expired(),
            _ => {}
        }
        outcome
    }

    async fn dispatch(
        &self,
        stream: &StreamId,
        request: Request,
    ) -> Result<Response, DeliveryError> {
        let live = self.store.get(stream).ok_or(DeliveryError::UnknownStream)?;
        match request.resource {
            Resource::Multivariant => self.multivariant(stream, &live),
            Resource::MediaPlaylist(rendition, _) => {
                self.media_playlist(stream, &live, rendition, request.resource, request.blocking)
                    .await
            }
            Resource::Initialization(_, initialization, _) => {
                let snapshot = self.rendition_for(&live, request.resource)?;
                let held = snapshot
                    .initialization_for(initialization)
                    .ok_or(DeliveryError::UnknownResource)?;
                Ok(Response::immutable(
                    MediaBody::single(held.payload.clone()),
                    &request.resource,
                )?)
            }
            Resource::Segment(rendition, segment, _) => {
                // Resolved for its claim rather than its contents: a completed
                // segment's bytes are held by the stream, not the snapshot.
                self.rendition_for(&live, request.resource)?;
                let stored = live
                    .segment(rendition, segment)
                    .ok_or(DeliveryError::UnknownResource)?;
                Ok(Response::immutable(
                    MediaBody::from_segment(&stored),
                    &request.resource,
                )?)
            }
            Resource::Part(rendition, part, _) => {
                self.part(&live, request.resource, rendition, part).await
            }
        }
    }

    fn multivariant(
        &self,
        stream_id: &StreamId,
        live: &Arc<LiveStream>,
    ) -> Result<Response, DeliveryError> {
        let stream = live.snapshot();
        // A topology exists only once a publisher has attached; the playlist is
        // servable from that instant, before any media is measured.
        if stream.presentation.is_none() {
            return Err(DeliveryError::UnknownResource);
        }
        let caches = self.cache_for(stream_id);
        // The base is configuration rather than request context, so one render
        // still answers every viewer of this stream.
        let uris = self.config.uri_base.uris(stream_id);
        let rendered = caches.multivariant().get_or_render(
            PlaylistKey::multivariant(&stream),
            || -> Result<String, DeliveryError> {
                multivariant_playlist(&stream, &self.config.playlist, &uris)?
                    .ok_or(DeliveryError::UnknownResource)
            },
        )?;
        self.meters.playlist_served(rendered.freshly_rendered);
        Ok(Response::playlist(rendered.bytes))
    }

    async fn media_playlist(
        &self,
        stream_id: &StreamId,
        live: &Arc<LiveStream>,
        rendition: RenditionId,
        resource: Resource,
        blocking: Option<BlockingReload>,
    ) -> Result<Response, DeliveryError> {
        let snapshot = self.rendition_for(live, resource)?;
        let deadline = blocking_reload_deadline(snapshot.contract, self.config.timing);

        if let Some(blocking) = blocking {
            // A terminal playlist will never advance, so a directive naming
            // media beyond its end is not an error and not something to wait
            // for; it is simply already answered.
            if !snapshot.live_edge.ended {
                self.validate_blocking(&snapshot, blocking)?;
                self.meters.blocking_reload_started();
                self.wait_for(live, rendition, deadline, |edge| {
                    edge.ended || satisfies(edge, blocking)
                })
                .await?;
            }
        } else if !playlist_is_ready(self.config.readiness, &snapshot.live_edge) {
            // The first request for a stream that has published nothing yet:
            // hold it rather than answer with a playlist naming no media.
            let readiness = self.config.readiness;
            self.wait_for(live, rendition, deadline, move |edge| {
                playlist_is_ready(readiness, edge)
            })
            .await?;
        }

        let stream = live.snapshot();
        let snapshot = self.rendition_for(live, resource)?;
        let caches = self.cache_for(stream_id);
        let uris = self.config.uri_base.uris(stream_id);
        let rendered = caches.rendition(rendition).get_or_render(
            PlaylistKey::media(&stream),
            || -> Result<String, DeliveryError> {
                let control = project::presentation_server_control(&stream, self.config.timing);
                Ok(media_playlist(
                    &stream,
                    &snapshot,
                    control,
                    &self.config.playlist,
                    &uris,
                )?)
            },
        )?;
        self.meters.playlist_served(rendered.freshly_rendered);
        Ok(Response::playlist(rendered.bytes))
    }

    /// Serves one partial segment, waiting if it has been hinted but not yet
    /// published.
    ///
    /// Holding the request is what a preload hint is *for*: HLS forbids sending
    /// a partial segment before the whole of it can be delivered at link speed,
    /// so there is nothing to stream early, and the client having its request
    /// already in flight is the entire latency saving.
    async fn part(
        &self,
        live: &Arc<LiveStream>,
        resource: Resource,
        rendition: RenditionId,
        part: PartId,
    ) -> Result<Response, DeliveryError> {
        let snapshot = self.rendition_for(live, resource)?;
        if let Some(stored) = live.part(rendition, part) {
            return Response::immutable(MediaBody::single(stored.payload.clone()), &resource);
        }

        // Absent means one of two things, and they get opposite answers: media
        // that has not been produced yet is worth waiting for, while media that
        // expired is gone for good.
        let hinted = snapshot
            .live_edge
            .next_part_id
            .is_some_and(|next| part >= next);
        if !hinted {
            return Err(DeliveryError::UnknownResource);
        }
        let deadline = blocking_reload_deadline(snapshot.contract, self.config.timing);
        self.wait_for(live, rendition, deadline, |edge| {
            edge.ended
                || edge
                    .last_part
                    .is_some_and(|(_, published)| published >= part)
        })
        .await?;

        let stored = live
            .part(rendition, part)
            .ok_or(DeliveryError::UnknownResource)?;
        Response::immutable(MediaBody::single(stored.payload.clone()), &resource)
    }

    /// Rejects a directive naming media so far ahead it cannot be a wait.
    ///
    /// The distinction matters: parking a request on media that will exist in
    /// a moment is the protocol working, while parking one on media a hundred
    /// segments away is a client bug that would otherwise consume a connection
    /// until the deadline expired.
    fn validate_blocking(
        &self,
        snapshot: &RenditionSnapshot,
        blocking: BlockingReload,
    ) -> Result<(), DeliveryError> {
        let edge = &snapshot.live_edge;
        let last_msn = edge
            .last_part
            .map(|(cursor, _)| cursor.msn)
            .or(edge.last_segment.map(|(msn, _)| msn))
            .unwrap_or(Msn(0));
        if blocking.msn.0 > last_msn.0.saturating_add(ADVANCE_SEGMENT_LIMIT) {
            return Err(DeliveryError::InvalidDirective(
                "_HLS_msn is too far beyond the live edge",
            ));
        }
        if let Some(part) = blocking.part {
            // Only meaningful within the segment the edge is in: a part index
            // in a *later* segment is bounded by the sequence-number check
            // above, since a segment's part count is not known in advance.
            if let Some((cursor, _)) = edge.last_part
                && cursor.msn == blocking.msn
                && advance_part_limit_exceeded(
                    cursor.part_index,
                    part,
                    snapshot
                        .contract
                        .part_target
                        .expect("a rendition with parts has a part target"),
                )
            {
                return Err(DeliveryError::InvalidDirective(
                    "_HLS_part is too far beyond the live edge",
                ));
            }
        }
        Ok(())
    }

    async fn wait_for(
        &self,
        live: &Arc<LiveStream>,
        rendition: RenditionId,
        deadline: Duration,
        ready: impl Fn(&RenditionLiveEdge) -> bool,
    ) -> Result<(), DeliveryError> {
        let mut updates = live
            .subscribe_rendition(rendition)
            .ok_or(DeliveryError::UnknownRendition)?;
        let wait = async {
            loop {
                if ready(&updates.borrow_and_update()) {
                    return Ok(());
                }
                if updates.changed().await.is_err() {
                    // The rendition is gone; whatever was being waited for is
                    // never arriving.
                    return Err(DeliveryError::UnknownResource);
                }
            }
        };
        timeout(deadline, wait)
            .await
            .unwrap_or(Err(DeliveryError::Unsatisfied))
    }

    /// The media of the rendition a name asks for, once that name's claim about
    /// it holds.
    ///
    /// The one place a claim is checked, because here is the first place it can
    /// be: the router parses a name before knowing which rendition, if any,
    /// answers it. A playlist called `audio.m3u8` really does carry audio and a
    /// `.m4s` really is CMAF — or this is a miss. Answering anyway would be
    /// worse than one: every format shares a single identifier space, so a
    /// cache would keep those bytes under a name no player should have fetched.
    fn rendition_for(
        &self,
        live: &Arc<LiveStream>,
        resource: Resource,
    ) -> Result<Arc<RenditionSnapshot>, DeliveryError> {
        let rendition = resource.rendition().ok_or(DeliveryError::UnknownResource)?;
        let stream = live.snapshot();
        let entry = rendition_entry(&stream, rendition).ok_or(DeliveryError::UnknownRendition)?;
        let holds = match resource {
            Resource::Multivariant => false,
            Resource::MediaPlaylist(_, kind) => entry.media.kind() == kind,
            Resource::Initialization(_, _, format)
            | Resource::Segment(_, _, format)
            | Resource::Part(_, _, format) => entry.contract.segment_format == format,
        };
        holds
            .then(|| entry.snapshot())
            .ok_or(DeliveryError::UnknownResource)
    }
}

fn playlist_is_ready(readiness: PlaylistReadiness, edge: &RenditionLiveEdge) -> bool {
    edge.ended
        || match readiness {
            PlaylistReadiness::CompletedSegment => edge.last_segment.is_some(),
            PlaylistReadiness::AnyMedia => edge.last_segment.is_some() || edge.last_part.is_some(),
        }
}

/// Whether a requested part is beyond HLS's Advance Part Limit.
///
/// The limit is three seconds' worth of parts when PART-TARGET is below one
/// second, otherwise three parts. Comparing durations directly preserves the
/// draft's fractional result without inventing a rounding rule.
fn advance_part_limit_exceeded(
    last: PartIndex,
    requested: PartIndex,
    part_target: Duration,
) -> bool {
    let advance = requested.0.saturating_sub(last.0);
    if part_target < Duration::from_secs(1) {
        part_target.saturating_mul(advance) > ADVANCE_PART_WINDOW
    } else {
        advance > 3
    }
}

/// Whether the edge has reached the position a directive names.
///
/// The rollover case falls out rather than being special-cased: a client asking
/// for a part index its segment never reached is satisfied when that segment
/// completes, because every part the segment did have is then available and the
/// next one belongs to the following media sequence number.
fn satisfies(edge: &RenditionLiveEdge, blocking: BlockingReload) -> bool {
    match blocking.part {
        None => edge
            .last_segment
            .is_some_and(|(msn, _)| msn >= blocking.msn),
        Some(part) => {
            edge.last_part.is_some_and(|(cursor, _)| {
                cursor.msn > blocking.msn
                    || (cursor.msn == blocking.msn && cursor.part_index >= part)
            }) || edge
                .last_segment
                .is_some_and(|(msn, _)| msn >= blocking.msn)
        }
    }
}

/// Looks one rendition up in a stream catalog.
pub fn rendition_entry(
    stream: &StreamSnapshot,
    rendition: RenditionId,
) -> Option<&RenditionCatalogEntry> {
    stream
        .renditions
        .iter()
        .find(|entry| entry.rendition_id == rendition)
}

#[cfg(test)]
mod tests {
    use crate::{
        delivery::hls::{
            InitializationId, SegmentId,
            fixtures::{
                PART_BYTES, chunk, initialization, lease, stream_id, video, write, write_segment,
            },
        },
        domain::MediaKind,
        mux::MediaSegmentFormat,
    };

    use super::*;

    /// Every fixture rendition is CMAF, so its names claim that packaging.
    const CMAF: MediaSegmentFormat = MediaSegmentFormat::Cmaf;

    fn origin(store: &StreamStore) -> Origin {
        Origin::new(store.clone(), DeliveryConfig::default())
    }

    /// A request carrying no delivery directives.
    fn fetch(resource: Resource) -> Request {
        Request::new(resource, None)
    }

    /// The fixture's video rendition, reloaded with whatever directives.
    fn video_playlist(blocking: Option<BlockingReload>) -> Request {
        Request::new(
            Resource::MediaPlaylist(RenditionId(0), MediaKind::Video),
            blocking,
        )
    }

    fn playlist(response: &Response) -> &str {
        match &response.body {
            Body::Playlist(bytes) => std::str::from_utf8(bytes).expect("a playlist is valid UTF-8"),
            Body::Media(_) => panic!("expected a playlist"),
        }
    }

    fn media(response: &Response) -> &MediaBody {
        match &response.body {
            Body::Media(body) => body,
            Body::Playlist(_) => panic!("expected media"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_stream_or_rendition_is_distinguished_from_expired_media() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        assert_eq!(
            origin
                .serve(&StreamId::new("nobody/here"), fetch(Resource::Multivariant))
                .await
                .unwrap_err(),
            DeliveryError::UnknownStream
        );
        assert_eq!(
            origin
                .serve(
                    &stream_id(),
                    fetch(Resource::Segment(RenditionId(9), SegmentId(1), CMAF))
                )
                .await
                .unwrap_err(),
            DeliveryError::UnknownRendition
        );
        assert_eq!(
            origin
                .serve(
                    &stream_id(),
                    fetch(Resource::Segment(RenditionId(0), SegmentId(99), CMAF))
                )
                .await
                .unwrap_err(),
            DeliveryError::UnknownResource
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_misdescribes_its_rendition_is_a_miss() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let playlist = |kind| fetch(Resource::MediaPlaylist(RenditionId(0), kind));
        let segment = |format| fetch(Resource::Segment(RenditionId(0), SegmentId(1), format));

        assert!(
            origin
                .serve(&stream_id(), playlist(MediaKind::Video))
                .await
                .is_ok()
        );
        assert!(origin.serve(&stream_id(), segment(CMAF)).await.is_ok());

        assert_eq!(
            origin
                .serve(&stream_id(), playlist(MediaKind::Audio))
                .await
                .unwrap_err(),
            DeliveryError::UnknownResource,
            "answering under a name that misdescribes the playlist would let a \
             client cache the lie"
        );
        assert_eq!(
            origin
                .serve(&stream_id(), segment(MediaSegmentFormat::WebVtt))
                .await
                .unwrap_err(),
            DeliveryError::UnknownResource,
            "every format shares one identifier space, so `.vtt` and `.m4s` \
             name different resources and only one of them exists"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_chunked_segment_is_served_from_the_parts_that_composed_it() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let response = origin
            .serve(
                &stream_id(),
                fetch(Resource::Segment(RenditionId(0), SegmentId(1), CMAF)),
            )
            .await
            .expect("the segment is retained");

        assert_eq!(response.caching, Caching::Immutable);
        assert_eq!(response.content_type, "video/iso.segment");
        assert_eq!(
            media(&response).frames().len(),
            6,
            "the six stored parts are sent as they are, not copied into one buffer"
        );
        assert_eq!(media(&response).len(), 6 * PART_BYTES as u64);
    }

    #[tokio::test(start_paused = true)]
    async fn a_byte_range_is_cut_across_the_buffers_it_spans() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let response = origin
            .serve(
                &stream_id(),
                fetch(Resource::Segment(RenditionId(0), SegmentId(1), CMAF)),
            )
            .await
            .expect("the segment is retained");
        let whole = media(&response);
        let clipped = whole.range(1_000, 2_100);

        assert_eq!(clipped.len(), 1_101);
        assert_eq!(
            clipped
                .frames()
                .iter()
                .map(|frame| frame.len())
                .collect::<Vec<_>>(),
            vec![24, 1_024, 53],
            "a range spanning three stored parts is clipped at both ends and \
             keeps the buffers between them, rather than being flattened"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_playlist_waits_for_media_rather_than_naming_none() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));

        let request = video_playlist(None);
        let held = tokio::spawn({
            let origin = origin.clone();
            async move { origin.serve(&stream_id(), request).await }
        });
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!held.is_finished(), "nothing has completed yet");

        write_segment(&lease, 0, 0, 0);
        let response = held.await.expect("the task ran").expect("media arrived");

        assert!(playlist(&response).contains("#EXTINF:6,\nsegment/1.m4s\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn any_media_readiness_wakes_on_the_first_part() {
        let store = StreamStore::default();
        let origin = Origin::new(
            store.clone(),
            DeliveryConfig {
                readiness: PlaylistReadiness::AnyMedia,
                ..DeliveryConfig::default()
            },
        );
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));

        let held = tokio::spawn({
            let origin = origin.clone();
            async move { origin.serve(&stream_id(), video_playlist(None)).await }
        });
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!held.is_finished(), "the playlist still names no media");

        write(&lease, chunk(0, 0, 0, 0));
        let response = held.await.expect("the task ran").expect("a part arrived");
        let rendered = playlist(&response);

        assert!(rendered.contains("#EXT-X-PART:DURATION=1,"));
        assert!(
            !rendered.contains("#EXTINF"),
            "AnyMedia does not wait for the parent segment to complete"
        );
    }

    #[test]
    fn the_advance_part_limit_tracks_the_part_target() {
        let half_second = Duration::from_millis(500);
        assert!(!advance_part_limit_exceeded(
            PartIndex(0),
            PartIndex(6),
            half_second
        ));
        assert!(advance_part_limit_exceeded(
            PartIndex(0),
            PartIndex(7),
            half_second
        ));

        let one_second = Duration::from_secs(1);
        assert!(!advance_part_limit_exceeded(
            PartIndex(4),
            PartIndex(7),
            one_second
        ));
        assert!(advance_part_limit_exceeded(
            PartIndex(4),
            PartIndex(8),
            one_second
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocking_reload_is_answered_the_moment_its_part_is_published() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        write(&lease, chunk(0, 1, 0, 6));

        let request = video_playlist(
            BlockingReload::from_directives(Some(1), Some(1))
                .expect("the directive is well formed"),
        );
        let held = tokio::spawn({
            let origin = origin.clone();
            async move { origin.serve(&stream_id(), request).await }
        });
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            !held.is_finished(),
            "part 1 of MSN 1 has not been published"
        );

        write(&lease, chunk(0, 1, 1, 7));
        let response = held.await.expect("the task ran").expect("the part arrived");

        assert!(playlist(&response).contains("URI=\"part/8.m4s\""));
    }

    #[tokio::test(start_paused = true)]
    async fn an_already_satisfied_directive_does_not_wait_at_all() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let response = origin
            .serve(
                &stream_id(),
                video_playlist(
                    BlockingReload::from_directives(Some(0), Some(5))
                        .expect("the directive is well formed"),
                ),
            )
            .await
            .expect("the requested position is already published");

        assert!(playlist(&response).contains("#EXTINF:6,"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsatisfied_reload_gives_up_after_three_target_durations() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let error = origin
            .serve(
                &stream_id(),
                video_playlist(
                    BlockingReload::from_directives(Some(2), None)
                        .expect("the directive is well formed"),
                ),
            )
            .await
            .expect_err("the publisher never produced it");

        assert_eq!(error, DeliveryError::Unsatisfied);
    }

    #[tokio::test(start_paused = true)]
    async fn directives_naming_impossible_positions_are_refused_rather_than_awaited() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        assert_eq!(
            BlockingReload::from_directives(None, Some(3)).unwrap_err(),
            DeliveryError::InvalidDirective("_HLS_part requires _HLS_msn"),
            "a part index with no segment names no position"
        );
        assert!(matches!(
            origin
                .serve(
                    &stream_id(),
                    video_playlist(BlockingReload::from_directives(Some(99), None).unwrap())
                )
                .await
                .unwrap_err(),
            DeliveryError::InvalidDirective(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_terminal_playlist_answers_directives_beyond_its_end_immediately() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        lease.end();

        let response = origin
            .serve(
                &stream_id(),
                video_playlist(BlockingReload::from_directives(Some(50), Some(9)).unwrap()),
            )
            .await
            .expect("an ended playlist is already final");

        assert!(playlist(&response).contains("#EXT-X-ENDLIST"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_hinted_part_is_held_until_it_exists_while_an_expired_one_is_not() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write(&lease, chunk(0, 0, 0, 0));

        // Part 2 is the one the playlist hints; it has not been published yet.
        let held = tokio::spawn({
            let origin = origin.clone();
            async move {
                origin
                    .serve(
                        &stream_id(),
                        fetch(Resource::Part(RenditionId(0), PartId(2), CMAF)),
                    )
                    .await
            }
        });
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!held.is_finished());

        write(&lease, chunk(0, 0, 1, 1));
        let response = held.await.expect("the task ran").expect("the part arrived");
        assert_eq!(media(&response).len(), 1_024);

        assert_eq!(
            origin
                .serve(
                    &stream_id(),
                    fetch(Resource::Part(RenditionId(0), PartId(1_000), CMAF))
                )
                .await
                .unwrap_err(),
            DeliveryError::Unsatisfied,
            "a part far beyond the hint is still a wait, bounded by the deadline"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_initialization_is_served_for_its_own_format() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));

        let response = origin
            .serve(
                &stream_id(),
                fetch(Resource::Initialization(
                    RenditionId(0),
                    InitializationId(1),
                    CMAF,
                )),
            )
            .await
            .expect("the header is retained");

        assert_eq!(response.content_type, "video/mp4");
        assert_eq!(response.caching, Caching::Immutable);
    }
}

#[cfg(test)]
mod cache_tests {
    use crate::{
        delivery::hls::fixtures::{
            chunk, initialization, lease, stream_id, video, write, write_segment,
        },
        domain::MediaKind,
    };

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn viewers_between_publications_share_one_render() {
        let store = StreamStore::default();
        let origin = Origin::new(store.clone(), DeliveryConfig::default());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let request = || {
            Request::new(
                Resource::MediaPlaylist(RenditionId(0), MediaKind::Video),
                None,
            )
        };
        for _ in 0..5 {
            origin
                .serve(&stream_id(), request())
                .await
                .expect("the playlist is servable");
        }

        let before = origin.meters().snapshot();
        assert_eq!(before.playlists_served, 5);
        assert_eq!(
            before.playlists_rendered, 1,
            "nothing changed between the five requests, so four of them are \
             refcounts rather than renders"
        );

        // Publishing moves the live edge, which must invalidate the cache.
        write(&lease, chunk(0, 1, 0, 6));
        origin
            .serve(&stream_id(), request())
            .await
            .expect("the playlist is servable");

        let after = origin.meters().snapshot();
        assert_eq!(after.playlists_rendered, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retired_stream_does_not_leave_its_cache_behind() {
        let store = StreamStore::default();
        let origin = Origin::new(store.clone(), DeliveryConfig::default());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        origin
            .serve(
                &stream_id(),
                Request::new(
                    Resource::MediaPlaylist(RenditionId(0), MediaKind::Video),
                    None,
                ),
            )
            .await
            .expect("the playlist is servable");

        assert_eq!(origin.prune(), 0, "the stream is still live");

        drop(lease);
        tokio::time::advance(store.limits().idle_retention + Duration::from_secs(1)).await;
        store.maintain();

        assert_eq!(
            origin.prune(),
            1,
            "a cache entry per stream retained forever is a leak with the \
             lifetime of the process"
        );
    }
}
