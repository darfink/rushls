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

pub use crate::delivery::{Body, DeliveryError, DeliveryFailure, Response};
use parking_lot::Mutex;

use crate::{
    delivery::hls::{
        LiveStream, Msn, PartIndex, PlaylistContract, RenditionCatalogEntry, RenditionLiveEdge,
        RenditionSnapshot, StreamSnapshot,
        cache::{PlaylistKey, StreamPlaylistCache},
        cache_control::CacheControlPolicy,
        project::{
            self, DeliveryTimingPolicy, PlaylistDelta, PlaylistPolicy, ProjectionError,
            media::media_playlist, multivariant::multivariant_playlist,
            timing::blocking_reload_deadline,
        },
        uri::{Resource, TOKEN_QUERYPARAM, UriBase, parse_path},
    },
    delivery::{EdgeCondition, Origin, Reuse, uri::ContentType},
    domain::{RenditionId, StreamId},
    observe::HlsMeters,
};

const ADVANCE_SEGMENT_LIMIT: u64 = 2;
const ADVANCE_PART_WINDOW: Duration = Duration::from_secs(3);
const HLS_CONTENT_TYPE: ContentType = ContentType::Manifest("application/vnd.apple.mpegurl");

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

/// One HLS condition evaluated by the origin's shared rendition wait loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitUntil {
    AnyMedia,
    CompletedSegment,
    IFramePosition(u64),
    PlaylistPosition { msn: Msn, part: Option<PartIndex> },
}

impl EdgeCondition for WaitUntil {
    fn reached(&self, edge: &RenditionLiveEdge) -> bool {
        match *self {
            Self::IFramePosition(msn) => edge.last_iframe.is_some_and(|last| last >= msn),
            Self::AnyMedia => edge.last_segment.is_some() || edge.last_part.is_some(),
            Self::CompletedSegment => edge.last_segment.is_some(),
            Self::PlaylistPosition { msn, part: None } => {
                edge.last_segment.is_some_and(|(last, _)| last >= msn)
            }
            Self::PlaylistPosition {
                msn,
                part: Some(part),
            } => {
                edge.last_part.is_some_and(|(cursor, _)| {
                    cursor.msn > msn || (cursor.msn == msn && cursor.part_index >= part)
                }) || edge.last_segment.is_some_and(|(last, _)| last >= msn)
            }
        }
    }
}

impl From<PlaylistReadiness> for WaitUntil {
    fn from(readiness: PlaylistReadiness) -> Self {
        match readiness {
            PlaylistReadiness::CompletedSegment => Self::CompletedSegment,
            PlaylistReadiness::AnyMedia => Self::AnyMedia,
        }
    }
}

/// Everything the request path needs that is not in the store.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Config {
    pub playlist: PlaylistPolicy,
    pub timing: DeliveryTimingPolicy,
    pub readiness: PlaylistReadiness,
    /// How long each class of response may be reused by downstream caches.
    pub cache_control: CacheControlPolicy,
    /// Where the names playlists emit are rooted. Relative by default.
    pub uri_base: UriBase,
    /// When set, a non-empty `token` query selects the QUERYPARAM playlist form.
    ///
    /// HTTP verifies the JWT. This flag only chooses which cached bytes to
    /// serve: native HLS clients cannot set `Authorization` on media fetches,
    /// so the playlist has to name `?token={$token}` itself.
    pub query_variables: bool,
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
/// A manifest [`Resource`] and, where one applies, the position the client
/// refuses to be answered before. A media-playlist name carries the media kind
/// it claims, which is checked against the rendition before projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub resource: Resource,
    /// The playlist-reload directive carried by this manifest request.
    pub blocking: Option<BlockingReload>,
    /// Playlist Delta Update request. Ignored on the multivariant playlist.
    pub skip: PlaylistDelta,
    /// Whether this request should be answered with the QUERYPARAM playlist.
    pub query_variables: bool,
}

impl Request {
    pub fn new(resource: Resource, blocking: Option<BlockingReload>) -> Self {
        Self {
            resource,
            blocking,
            skip: PlaylistDelta::Full,
            query_variables: false,
        }
    }

    #[must_use]
    pub fn with_skip(mut self, skip: PlaylistDelta) -> Self {
        self.skip = skip;
        self
    }

    #[must_use]
    pub fn with_query_variables(mut self, query_variables: bool) -> Self {
        self.query_variables = query_variables;
        self
    }
}

impl From<ProjectionError> for DeliveryError {
    fn from(_: ProjectionError) -> Self {
        Self::Projection
    }
}

/// The set of streams this origin serves.
#[derive(Clone, Debug)]
pub struct Service {
    origin: Arc<Origin>,
    config: Config,
    meters: HlsMeters,
    /// Rendered playlists, per stream.
    ///
    /// Held here rather than in the store because a rendered playlist is a
    /// projection concern and the store is deliberately ignorant of playlists.
    /// Entries outlive their stream until [`Self::remove_streams`] receives the
    /// retired identities from the store's maintenance tick.
    caches: Arc<Mutex<HashMap<StreamId, Arc<StreamPlaylistCache>>>>,
}

impl Service {
    pub fn new(origin: Arc<Origin>, config: Config) -> Self {
        Self::with_meters(origin, config, HlsMeters::default())
    }

    pub fn with_meters(origin: Arc<Origin>, config: Config, meters: HlsMeters) -> Self {
        Self {
            origin,
            config,
            meters,
            caches: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn meters(&self) -> &HlsMeters {
        &self.meters
    }

    /// How long an HLS request may wait for media at this cadence.
    ///
    /// Kept on the adapter because the deadline is an HLS delivery policy even
    /// when the object being waited for has a protocol-neutral media path.
    pub fn media_deadline(&self, contract: PlaylistContract) -> Duration {
        blocking_reload_deadline(contract, self.config.timing)
    }

    /// How long immutable media may be reused at this rendition's cadence.
    pub fn media_reuse(&self, target: Option<Duration>) -> Reuse {
        self.config.cache_control.media(target)
    }

    /// How long an absent resource may be remembered by an HLS-facing cache.
    pub fn missing_reuse(&self, target: Option<Duration>, blocking: bool) -> Reuse {
        self.config.cache_control.missing(target, blocking)
    }

    /// Resolves and serves one HLS manifest request target.
    pub async fn serve_path(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<Response, DeliveryFailure> {
        let Ok(named) = parse_path(path) else {
            return Err(self.unrouted(DeliveryError::UnknownResource));
        };
        let directives = match parse_directives(query) {
            Ok(directives) => directives,
            Err(error) => return Err(self.unrouted(error)),
        };
        self.serve(
            &named.stream,
            Request::new(named.resource, directives.blocking)
                .with_skip(directives.skip)
                .with_query_variables(
                    self.config.query_variables && query_has_nonempty_token(query),
                ),
        )
        .await
    }

    /// Drops cached manifests for streams the store just retired.
    pub fn remove_streams<'a>(&self, streams: impl IntoIterator<Item = &'a StreamId>) -> usize {
        let mut caches = self.caches.lock();
        let before = caches.len();
        for stream in streams {
            caches.remove(stream);
        }
        before - caches.len()
    }

    fn cache_for(&self, stream: &StreamId) -> Arc<StreamPlaylistCache> {
        let mut caches = self.caches.lock();
        if let Some(cache) = caches.get(stream) {
            return Arc::clone(cache);
        }
        let cache = Arc::new(StreamPlaylistCache::new(self.config.uri_base.uris(stream)));
        caches.insert(stream.clone(), Arc::clone(&cache));
        cache
    }

    pub async fn serve(
        &self,
        stream: &StreamId,
        request: Request,
    ) -> Result<Response, DeliveryFailure> {
        let outcome = self.dispatch(stream, request).await;
        match &outcome {
            Err(DeliveryError::InvalidDirective(_)) => self.origin.meters().request_rejected(),
            Err(
                DeliveryError::UnknownStream
                | DeliveryError::UnknownRendition
                | DeliveryError::UnknownResource,
            ) => self.origin.meters().request_not_found(),
            Err(DeliveryError::Unsatisfied) => self.meters.blocking_reload_expired(),
            _ => {}
        }
        outcome.map_err(|error| self.failure(error, Some((stream, request))))
    }

    /// Describes a failure that never reached a stream, such as an unroutable
    /// path.
    pub fn unrouted(&self, error: DeliveryError) -> DeliveryFailure {
        self.failure(error, None)
    }

    /// Attaches to a failure how long anyone may remember it.
    ///
    /// Only absence is cacheable. A rejected directive is the client's to fix,
    /// and an unmet blocking deadline is a momentary condition — pinning one
    /// across a CDN would turn a single slow tick into target durations of
    /// failure for every viewer behind it. Both of those skip the store lookup
    /// entirely, because a lifetime nobody is granted needs no target duration.
    fn failure(
        &self,
        error: DeliveryError,
        request: Option<(&StreamId, Request)>,
    ) -> DeliveryFailure {
        let reuse = match error {
            DeliveryError::UnknownStream
            | DeliveryError::UnknownRendition
            | DeliveryError::UnknownResource => {
                let (target, blocking) = request.map_or((None, false), |(stream, request)| {
                    (
                        self.target_duration(stream, request.resource),
                        // Skip-only names the live edge, not one playlist state.
                        request.blocking.is_some(),
                    )
                });
                self.config.cache_control.missing(target, blocking)
            }
            _ => Reuse::revalidate(),
        };
        DeliveryFailure { error, reuse }
    }

    /// The target duration a lifetime for this request should be scaled by.
    ///
    /// Best effort by construction, because this also answers for requests that
    /// named nothing that exists: the rendition's own contract where there is
    /// one, the widest cadence in the presentation for a request that spans
    /// renditions, and otherwise nothing, which leaves the policy to assume.
    fn target_duration(&self, stream: &StreamId, resource: Resource) -> Option<Duration> {
        let stream = self.origin.stream(stream)?.snapshot();
        resource
            .rendition()
            .and_then(|rendition| rendition_entry(&stream, rendition))
            .map(|entry| target_duration_of(entry.contract))
            .or_else(|| longest_target_duration(&stream))
    }

    async fn dispatch(
        &self,
        stream: &StreamId,
        request: Request,
    ) -> Result<Response, DeliveryError> {
        let live = self
            .origin
            .stream(stream)
            .ok_or(DeliveryError::UnknownStream)?;
        match request.resource {
            Resource::Multivariant => self.multivariant(stream, &live, request.query_variables),
            Resource::IFramePlaylist(_) if !self.config.playlist.iframe_playlists => {
                Err(DeliveryError::UnknownResource)
            }
            Resource::MediaPlaylist(rendition, _) | Resource::IFramePlaylist(rendition) => {
                self.media_playlist(stream, &live, rendition, request).await
            }
        }
    }

    fn multivariant(
        &self,
        stream_id: &StreamId,
        live: &Arc<LiveStream>,
        query_variables: bool,
    ) -> Result<Response, DeliveryError> {
        let stream = live.snapshot();
        // A topology exists only once a publisher has attached; the playlist is
        // servable from that instant, before any media is measured.
        if stream.presentation.is_none() {
            return Err(DeliveryError::UnknownResource);
        }
        let caches = self.cache_for(stream_id);
        let rendered = caches.multivariant().get_or_render(
            PlaylistKey::multivariant(&stream).with_query_variables(query_variables),
            || -> Result<String, DeliveryError> {
                multivariant_playlist(&stream, &self.config.playlist, caches.uris(query_variables))?
                    .ok_or(DeliveryError::UnknownResource)
            },
        )?;
        self.meters.playlist_served(rendered.freshly_rendered);
        // A multivariant playlist names no rendition of its own, so it is paced
        // by the widest cadence it points at, and it is never the target of a
        // blocking reload.
        Ok(Response::manifest(
            rendered.bytes,
            rendered.gzip,
            HLS_CONTENT_TYPE,
            self.config
                .cache_control
                .playlist(longest_target_duration(&stream), false),
        ))
    }

    async fn media_playlist(
        &self,
        stream_id: &StreamId,
        live: &Arc<LiveStream>,
        rendition: RenditionId,
        request: Request,
    ) -> Result<Response, DeliveryError> {
        let snapshot = Self::rendition_for(live, request.resource)?;
        let deadline = blocking_reload_deadline(snapshot.contract, self.config.timing);
        let iframe = matches!(request.resource, Resource::IFramePlaylist(_));
        // No partial segments exist in this view. A part directive therefore
        // waits for its completed parent, not for the regular video's part.
        let blocking = request.blocking.map(|mut blocking| {
            if iframe {
                blocking.part = None;
            }
            blocking
        });

        if let Some(blocking) = blocking {
            // A terminal playlist will never advance, so a directive naming
            // media beyond its end is not an error and not something to wait
            // for; it is simply already answered.
            if !snapshot.live_edge.ended {
                let condition = if iframe {
                    Self::validate_iframe_blocking(&snapshot, blocking.msn.0)?;
                    WaitUntil::IFramePosition(blocking.msn.0)
                } else {
                    Self::validate_blocking(&snapshot, blocking)?;
                    WaitUntil::PlaylistPosition {
                        msn: blocking.msn,
                        part: blocking.part,
                    }
                };
                self.meters.blocking_reload_started();
                let _outcome = self
                    .origin
                    .wait_for(live, rendition, deadline, condition)
                    .await?;
            }
        } else {
            let readiness = if iframe {
                WaitUntil::CompletedSegment
            } else {
                WaitUntil::from(self.config.readiness)
            };
            if !readiness.reached(&snapshot.live_edge) && !snapshot.live_edge.ended {
                // The first request for a stream that has published nothing yet:
                // hold it rather than answer with a playlist naming no media.
                let _outcome = self
                    .origin
                    .wait_for(live, rendition, deadline, readiness)
                    .await?;
            }
        }

        let caches = self.cache_for(stream_id);
        let skip = request.skip;
        let cache = if iframe {
            caches.iframe(rendition)
        } else {
            caches.rendition(rendition)
        };
        let rendered = cache.get_or_render_stable(
            || -> Result<_, DeliveryError> {
                let media_revision = live.media_revision();
                let stream = live.snapshot();
                let snapshot = rendition_for_stream(&stream, request.resource)?;
                Ok((
                    PlaylistKey::media(&stream, media_revision, skip)
                        .with_query_variables(request.query_variables),
                    (stream, snapshot),
                ))
            },
            |(stream, snapshot)| -> Result<String, DeliveryError> {
                let control = project::presentation_server_control(stream, self.config.timing);
                let render = if iframe {
                    project::media::iframe_playlist
                } else {
                    media_playlist
                };
                Ok(render(
                    stream,
                    snapshot,
                    control,
                    &self.config.playlist,
                    caches.uris(request.query_variables),
                    skip,
                )?)
            },
        )?;
        self.meters.playlist_served(rendered.freshly_rendered);
        Ok(Response::manifest(
            rendered.bytes,
            rendered.gzip,
            HLS_CONTENT_TYPE,
            self.config.cache_control.playlist(
                Some(target_duration_of(snapshot.contract)),
                // Skip-only is still the live edge. Skip plus `_HLS_msn` /
                // `_HLS_part` names one playlist state.
                blocking.is_some(),
            ),
        ))
    }

    fn validate_iframe_blocking(
        snapshot: &RenditionSnapshot,
        msn: u64,
    ) -> Result<(), DeliveryError> {
        if msn
            > snapshot
                .live_edge
                .last_iframe
                .unwrap_or(0)
                .saturating_add(ADVANCE_SEGMENT_LIMIT)
        {
            return Err(DeliveryError::InvalidDirective(
                "_HLS_msn is too far beyond the I-frame live edge",
            ));
        }
        Ok(())
    }

    /// Rejects a directive naming media so far ahead it cannot be a wait.
    ///
    /// The distinction matters: parking a request on media that will exist in
    /// a moment is the protocol working, while parking one on media a hundred
    /// segments away is a client bug that would otherwise consume a connection
    /// until the deadline expired.
    fn validate_blocking(
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
        live: &Arc<LiveStream>,
        resource: Resource,
    ) -> Result<Arc<RenditionSnapshot>, DeliveryError> {
        let stream = live.snapshot();
        rendition_for_stream(&stream, resource)
    }
}

fn parse_directives(query: Option<&str>) -> Result<PlaylistQuery, DeliveryError> {
    let Some(query) = query else {
        return Ok(PlaylistQuery::default());
    };
    let mut msn = None;
    let mut part = None;
    let mut skip = PlaylistDelta::Full;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "_HLS_msn" => {
                msn =
                    Some(value.parse::<u64>().map_err(|_| {
                        DeliveryError::InvalidDirective("_HLS_msn must be a number")
                    })?);
            }
            "_HLS_part" => {
                part =
                    Some(value.parse::<u32>().map_err(|_| {
                        DeliveryError::InvalidDirective("_HLS_part must be a number")
                    })?);
            }
            "_HLS_skip" => {
                skip = match value {
                    "YES" => PlaylistDelta::Skip,
                    "v2" => PlaylistDelta::SkipV2,
                    // Unknown skip values are full playlists, not 400.
                    _ => PlaylistDelta::Full,
                };
            }
            // Unknown directives are forward-compatible protocol extensions,
            // not malformed requests.
            _ => {}
        }
    }
    Ok(PlaylistQuery {
        blocking: BlockingReload::from_directives(msn, part)?,
        skip,
    })
}

/// Whether the query carries a non-empty `token` parameter.
///
/// Delivery chooses the playlist form from this rather than from the verified
/// JWT, so the bytes and the cache key that names them are decided by the same
/// fact. It stays true when the header carried the token as well: native HLS
/// will fetch the children of that playlist without a header either way.
///
/// An empty `token=` is not this. The HTTP gate 401s it before delivery is
/// reached, and a playlist declaring `QUERYPARAM` is unparseable to a client
/// whose request has no value to substitute.
fn query_has_nonempty_token(query: Option<&str>) -> bool {
    query
        .and_then(|query| {
            query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .find_map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (key == TOKEN_QUERYPARAM).then_some(value)
                })
        })
        .is_some_and(|value| !value.is_empty())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PlaylistQuery {
    blocking: Option<BlockingReload>,
    skip: PlaylistDelta,
}

fn rendition_for_stream(
    stream: &StreamSnapshot,
    resource: Resource,
) -> Result<Arc<RenditionSnapshot>, DeliveryError> {
    let rendition = resource.rendition().ok_or(DeliveryError::UnknownResource)?;
    let entry = rendition_entry(stream, rendition).ok_or(DeliveryError::UnknownRendition)?;
    let holds = match resource {
        Resource::MediaPlaylist(_, kind) => entry.media.kind() == kind,
        Resource::IFramePlaylist(_) => {
            entry.media.kind() == crate::domain::MediaKind::Video
                && entry.contract.segment_format == crate::mux::MediaSegmentFormat::Cmaf
        }
        Resource::Multivariant => false,
    };
    holds
        .then(|| entry.snapshot())
        .ok_or(DeliveryError::UnknownResource)
}

/// Whether a requested part is beyond HLS's Advance Part Limit.
///
/// Draft-pantos-hls-rfc8216bis-22, section 6.2.5.2, defines the limit as
/// "three divided by the Part Target Duration if the Part Target Duration is
/// less than one second, or three otherwise" — a part-count ceiling that
/// stays fractional for sub-second targets. Comparing `advance × PART-TARGET`
/// against three seconds is the same comparison without a fractional
/// division, and the strict `>` matches the draft's "exceeds ... by the
/// Advance Part Limit".
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

/// The whole-second target duration a contract promised.
fn target_duration_of(contract: PlaylistContract) -> Duration {
    Duration::from_secs(contract.target_duration.get())
}

/// The widest cadence in the presentation.
///
/// What paces anything that belongs to no single rendition, for the same reason
/// one `EXT-X-SERVER-CONTROL` covers them all: a value that held for the
/// narrowest cadence would be wrong for every other rendition in the playlist.
fn longest_target_duration(stream: &StreamSnapshot) -> Option<Duration> {
    project::active_contracts(stream)
        .map(target_duration_of)
        .max()
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
        delivery::hls::fixtures::{
            audio, chunk, initialization, lease, stream_id, video, video_with_cadence, write,
            write_segment,
        },
        delivery::store::StreamStore,
        domain::MediaKind,
    };

    use super::*;

    fn origin(store: &StreamStore) -> Service {
        Service::new(Arc::new(Origin::new(store.clone())), Config::default())
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
            Body::Manifest(bytes) => std::str::from_utf8(bytes).expect("a playlist is valid UTF-8"),
            Body::Media(_) => panic!("expected a playlist"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn iframe_routes_are_opt_in_and_have_separate_token_and_video_caches()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::delivery::hls::fixtures::write_cmaf_segment;
        let store = StreamStore::default();
        let lease = lease(&store, vec![video(0), audio(1)]);
        write(&lease, initialization(0, 1));
        write_cmaf_segment(&lease, 0, 0, 0)?;
        let iframe = Resource::IFramePlaylist(RenditionId(0));
        assert_eq!(
            origin(&store)
                .serve(&stream_id(), fetch(iframe))
                .await
                .unwrap_err()
                .error,
            DeliveryError::UnknownResource
        );
        let service = Service::new(
            Arc::new(Origin::new(store)),
            Config {
                playlist: PlaylistPolicy {
                    iframe_playlists: true,
                    ..PlaylistPolicy::default()
                },
                query_variables: true,
                ..Config::default()
            },
        );
        assert_eq!(
            service
                .serve(
                    &stream_id(),
                    fetch(Resource::IFramePlaylist(RenditionId(1)))
                )
                .await
                .unwrap_err()
                .error,
            DeliveryError::UnknownResource
        );
        for _ in 0..3 {
            for query in [false, true] {
                let result = service
                    .serve(&stream_id(), fetch(iframe).with_query_variables(query))
                    .await
                    .map_err(|failure| failure.error)?;
                assert!(playlist(&result).contains("#EXT-X-I-FRAMES-ONLY"));
                assert_eq!(playlist(&result).contains("token={$token}"), query);
                let normal = service
                    .serve(
                        &stream_id(),
                        video_playlist(None).with_query_variables(query),
                    )
                    .await
                    .map_err(|failure| failure.error)?;
                assert!(!playlist(&normal).contains("#EXT-X-I-FRAMES-ONLY"));
                assert!(playlist(&normal).contains("#EXT-X-PART:"));
            }
        }
        assert_eq!(service.meters().snapshot().playlists_rendered, 4);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn iframe_blocking_reload_waits_for_the_completed_parent()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            delivery::hls::fixtures::{cmaf_fragment, write_cmaf_segment_with_keyframes},
            mux::{
                PackagedMedia, PackagedSegmentCompletion, PackagingRenditionId, PackagingSegmentId,
            },
        };
        let store = StreamStore::default();
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_cmaf_segment_with_keyframes(&lease, 0, 0, 0, &[0, 2, 4])?;
        let service = Service::new(
            Arc::new(Origin::new(store)),
            Config {
                playlist: PlaylistPolicy {
                    iframe_playlists: true,
                    ..PlaylistPolicy::default()
                },
                readiness: PlaylistReadiness::AnyMedia,
                ..Config::default()
            },
        );
        let request = Request::new(
            Resource::IFramePlaylist(RenditionId(0)),
            Some(BlockingReload {
                msn: Msn(3),
                part: Some(PartIndex(0)),
            }),
        );
        let held = tokio::spawn({
            let service = service.clone();
            async move { service.serve(&stream_id(), request).await }
        });
        tokio::task::yield_now().await;
        let PackagedMedia::Chunk(mut first) = chunk(0, 1, 0, 6) else {
            unreachable!()
        };
        first.payload = cmaf_fragment(true)?;
        write(&lease, PackagedMedia::Chunk(first));
        tokio::task::yield_now().await;
        assert!(
            !held.is_finished(),
            "an I-frame reload cannot wake on a regular video part"
        );
        for index in 1..6 {
            let PackagedMedia::Chunk(mut part) = chunk(0, 1, index, 6 + i64::from(index)) else {
                unreachable!()
            };
            part.payload = cmaf_fragment(false)?;
            write(&lease, PackagedMedia::Chunk(part));
        }
        write(
            &lease,
            PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                rendition_id: PackagingRenditionId(0),
                packaging_segment_id: PackagingSegmentId(1),
                media_start: 6,
                duration: 6,
            }),
        );
        let response = held.await?.map_err(|failure| failure.error)?;
        assert_eq!(playlist(&response).matches("#EXT-X-BYTERANGE:").count(), 4);
        lease.end();
        let ended = service
            .serve(
                &stream_id(),
                Request::new(
                    Resource::IFramePlaylist(RenditionId(0)),
                    Some(BlockingReload {
                        msn: Msn(99),
                        part: None,
                    }),
                ),
            )
            .await
            .map_err(|failure| failure.error)?;
        assert!(playlist(&ended).ends_with("#EXT-X-ENDLIST\n"));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_stream_is_reported() {
        let store = StreamStore::default();
        let origin = origin(&store);

        assert_eq!(
            origin
                .serve(&StreamId::new("nobody/here"), fetch(Resource::Multivariant))
                .await
                .unwrap_err()
                .error,
            DeliveryError::UnknownStream
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
        assert!(
            origin
                .serve(&stream_id(), playlist(MediaKind::Video))
                .await
                .is_ok()
        );

        assert_eq!(
            origin
                .serve(&stream_id(), playlist(MediaKind::Audio))
                .await
                .unwrap_err()
                .error,
            DeliveryError::UnknownResource,
            "answering under a name that misdescribes the playlist would let a \
             client cache the lie"
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
        let origin = Service::new(
            Arc::new(Origin::new(store.clone())),
            Config {
                readiness: PlaylistReadiness::AnyMedia,
                ..Config::default()
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

        let failure = origin
            .serve(
                &stream_id(),
                video_playlist(
                    BlockingReload::from_directives(Some(2), None)
                        .expect("the directive is well formed"),
                ),
            )
            .await
            .expect_err("the publisher never produced it");

        assert_eq!(failure.error, DeliveryError::Unsatisfied);
        assert_eq!(
            failure.reuse,
            Reuse::revalidate(),
            "a deadline the origin missed once must not be pinned across a CDN \
             for every viewer behind it"
        );
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
                .unwrap_err()
                .error,
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
    async fn a_playlist_naming_no_rendition_is_paced_by_the_widest_one_it_points_at() {
        let store = StreamStore::default();
        let origin = origin(&store);
        // Two seconds of video against six of audio: a multivariant playlist
        // reusable for half of the *shorter* cadence would still be describing
        // the audio rendition long after the video one moved on.
        let _lease = lease(&store, vec![video_with_cadence(0, 2, 1), audio(1)]);

        let response = origin
            .serve(&stream_id(), fetch(Resource::Multivariant))
            .await
            .expect("a presentation with a topology is servable");

        assert_eq!(
            response.reuse,
            CacheControlPolicy::default().playlist(Some(Duration::from_secs(6)), false)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_directive_makes_a_playlist_reusable_far_longer_than_the_live_edge_is() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        let policy = CacheControlPolicy::default();

        let edge = origin
            .serve(&stream_id(), video_playlist(None))
            .await
            .expect("the playlist names media");
        let position = origin
            .serve(
                &stream_id(),
                video_playlist(BlockingReload::from_directives(Some(0), None).unwrap()),
            )
            .await
            .expect("the requested position is already published");

        assert_eq!(
            edge.reuse,
            policy.playlist(Some(Duration::from_secs(6)), false)
        );
        assert_eq!(
            position.reuse,
            policy.playlist(Some(Duration::from_secs(6)), true)
        );
        assert!(
            position.reuse.max_age > edge.reuse.max_age,
            "`_HLS_msn` is part of the URL, so the same bytes stay the right \
             answer to it while a bare playlist URL means the moving edge"
        );
    }

    #[test]
    fn skip_query_values_are_yes_v2_or_the_full_playlist() {
        assert_eq!(
            parse_directives(Some("_HLS_skip=YES"))
                .expect("YES is well formed")
                .skip,
            PlaylistDelta::Skip
        );
        assert_eq!(
            parse_directives(Some("_HLS_skip=v2"))
                .expect("v2 is well formed")
                .skip,
            PlaylistDelta::SkipV2
        );
        assert_eq!(
            parse_directives(Some("_HLS_skip=NO"))
                .expect("unknown skip is not an error")
                .skip,
            PlaylistDelta::Full
        );
        assert_eq!(
            parse_directives(Some("_HLS_skip=yes"))
                .expect("the spec value is YES, not yes")
                .skip,
            PlaylistDelta::Full
        );
        let both = parse_directives(Some("_HLS_msn=1&_HLS_skip=YES")).expect("combined");
        assert_eq!(both.skip, PlaylistDelta::Skip);
        assert_eq!(both.blocking.map(|blocking| blocking.msn.0), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_skip_is_the_full_playlist_not_a_refusal() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let full = origin
            .serve(&stream_id(), video_playlist(None))
            .await
            .expect("the playlist names media");
        let unknown = origin
            .serve(
                &stream_id(),
                video_playlist(None).with_skip(PlaylistDelta::Full),
            )
            .await
            .expect("an unknown skip value is a full playlist");
        let via_query = origin
            .serve_path("/live/camera/0/video.m3u8", Some("_HLS_skip=nope"))
            .await
            .expect("unknown skip must not 400");

        assert_eq!(playlist(&full), playlist(&unknown));
        assert_eq!(playlist(&full), playlist(&via_query));
        assert!(!playlist(&via_query).contains("#EXT-X-SKIP"));
    }

    #[tokio::test(start_paused = true)]
    async fn skip_only_is_the_live_edge_and_skip_plus_msn_names_one_state() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        let policy = CacheControlPolicy::default();

        let skip_only = origin
            .serve(
                &stream_id(),
                video_playlist(None).with_skip(PlaylistDelta::Skip),
            )
            .await
            .expect("a skip request is answerable");
        let skip_and_block = origin
            .serve(
                &stream_id(),
                video_playlist(BlockingReload::from_directives(Some(0), None).unwrap())
                    .with_skip(PlaylistDelta::Skip),
            )
            .await
            .expect("blocking is waited first, then the delta is rendered");

        assert_eq!(
            skip_only.reuse,
            policy.playlist(Some(Duration::from_secs(6)), false)
        );
        assert_eq!(
            skip_and_block.reuse,
            policy.playlist(Some(Duration::from_secs(6)), true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn skip_only_absence_is_not_cached_as_a_blocking_miss() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let _lease = lease(&store, vec![video(0)]);
        let policy = CacheControlPolicy::default();

        let skip_only = origin
            .serve_path("/live/camera/9/video.m3u8", Some("_HLS_skip=YES"))
            .await
            .expect_err("no such rendition");
        let skip_and_block = origin
            .serve_path(
                "/live/camera/9/video.m3u8",
                Some("_HLS_skip=YES&_HLS_msn=1"),
            )
            .await
            .expect_err("no such rendition");

        assert_eq!(skip_only.error, DeliveryError::UnknownRendition);
        assert_eq!(
            skip_only.reuse,
            policy.missing(Some(Duration::from_secs(6)), false),
            "skip-only is the live edge, so a miss stays the short lifetime"
        );
        assert_eq!(
            skip_and_block.reuse,
            policy.missing(Some(Duration::from_secs(6)), true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_multivariant_playlist_ignores_skip() {
        let store = StreamStore::default();
        let origin = origin(&store);
        let _lease = lease(&store, vec![video(0)]);

        let index = origin
            .serve(
                &stream_id(),
                fetch(Resource::Multivariant).with_skip(PlaylistDelta::Skip),
            )
            .await
            .expect("a presentation with a topology is servable");

        assert!(!playlist(&index).contains("#EXT-X-SKIP"));
        assert!(!playlist(&index).contains("CAN-SKIP-UNTIL"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_skip_request_renders_a_delta_once_the_window_is_wide_enough() {
        let store = StreamStore::new(crate::delivery::hls::StoreLimits {
            retention: crate::delivery::hls::RetentionPolicy {
                retain: Duration::from_hours(2).into(),
                ..crate::delivery::hls::RetentionPolicy::default()
            },
            ..crate::delivery::hls::StoreLimits::default()
        });
        let origin = origin(&store);
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        for id in 0..10 {
            write_segment(
                &lease,
                0,
                id,
                i64::try_from(id).expect("fixture id fits i64") * 6,
            );
        }

        let full = origin
            .serve(&stream_id(), video_playlist(None))
            .await
            .expect("the full playlist is servable");
        let delta = origin
            .serve(
                &stream_id(),
                video_playlist(None).with_skip(PlaylistDelta::Skip),
            )
            .await
            .expect("the delta is servable");

        assert!(!playlist(&full).contains("#EXT-X-SKIP"));
        assert!(playlist(&delta).contains("#EXT-X-SKIP:SKIPPED-SEGMENTS=3"));
        assert!(playlist(&full).contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist(&delta).contains("#EXT-X-MEDIA-SEQUENCE:0"));
    }
}

#[cfg(test)]
mod cache_tests {
    #[cfg(feature = "allocation-counting")]
    use crate::test_alloc::count_async;
    use crate::{
        delivery::hls::fixtures::{
            chunk, initialization, lease, stream_id, video, write, write_segment,
        },
        delivery::store::StreamStore,
        domain::MediaKind,
    };

    use super::*;
    #[cfg(feature = "allocation-counting")]
    use crate::delivery::{store::PartId, uri::MediaResource};

    #[tokio::test(start_paused = true)]
    async fn viewers_between_publications_share_one_render() {
        let store = StreamStore::default();
        let origin = Service::new(Arc::new(Origin::new(store.clone())), Config::default());
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
    async fn full_and_delta_viewers_do_not_re_render_each_others_variant() {
        let store = StreamStore::default();
        let origin = Service::new(Arc::new(Origin::new(store.clone())), Config::default());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let full = Request::new(
            Resource::MediaPlaylist(RenditionId(0), MediaKind::Video),
            None,
        );
        let delta = full.with_skip(PlaylistDelta::Skip);
        for _ in 0..3 {
            origin
                .serve(&stream_id(), full)
                .await
                .expect("the playlist is servable");
            origin
                .serve(&stream_id(), delta)
                .await
                .expect("the playlist is servable");
        }

        let before = origin.meters().snapshot();
        assert_eq!(
            before.playlists_rendered, 2,
            "full and delta of one epoch occupy sibling slots"
        );

        write(&lease, chunk(0, 1, 0, 6));
        origin
            .serve(&stream_id(), full)
            .await
            .expect("the playlist is servable");
        origin
            .serve(&stream_id(), delta)
            .await
            .expect("the playlist is servable");

        let after = origin.meters().snapshot();
        assert_eq!(
            after.playlists_rendered, 4,
            "a live-edge advance invalidates both variants because each \
             carries EXT-X-RENDITION-REPORT"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_retired_stream_does_not_leave_its_cache_behind() {
        let store = StreamStore::default();
        let origin = Service::new(Arc::new(Origin::new(store.clone())), Config::default());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        assert!(
            lease.live().claim_availability(),
            "the direct-store fixture stands in for the publisher's inline announcement"
        );
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

        assert_eq!(
            origin.remove_streams(std::iter::empty()),
            0,
            "the stream is still live"
        );

        drop(lease);
        tokio::time::advance(
            store
                .limits()
                .retention
                .retain
                .resolve(Duration::from_secs(6))
                + Duration::from_secs(1),
        )
        .await;
        let retired = store.maintain().retired;

        assert_eq!(
            origin.remove_streams(&retired),
            1,
            "a cache entry per stream retained forever is a leak with the \
             lifetime of the process"
        );
    }

    #[cfg(feature = "allocation-counting")]
    #[tokio::test(start_paused = true)]
    async fn retained_parts_and_cached_playlists_allocate_nothing_per_viewer() {
        let store = StreamStore::default();
        let origin = Service::new(Arc::new(Origin::new(store.clone())), Config::default());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);
        let stream = stream_id();

        let media = Origin::new(store.clone());
        let part = MediaResource::Part(
            RenditionId(0),
            PartId(1),
            crate::mux::MediaSegmentFormat::Cmaf,
        );
        media
            .media(&stream, part, Duration::from_secs(18))
            .await
            .expect("warm the thread-local lock-free lookup state");
        let (response, allocations) =
            count_async(media.media(&stream, part, Duration::from_secs(18))).await;
        assert!(response.is_ok());
        assert_eq!(allocations, 0, "a retained part is handed out by refcount");

        let playlist = Request::new(
            Resource::MediaPlaylist(RenditionId(0), MediaKind::Video),
            None,
        );
        origin
            .serve(&stream, playlist)
            .await
            .expect("the first request warms the render cache");
        let (response, allocations) = count_async(origin.serve(&stream, playlist)).await;
        assert!(response.is_ok());
        assert_eq!(
            allocations, 0,
            "a cached playlist request only clones stable handles"
        );
    }

    #[test]
    fn hostile_reload_directives_never_panic() {
        let mut state = 0x41c6_ce57_edcf_a8b4_u64;
        for _ in 0..4_000 {
            let query = crate::test_fuzz::string(&mut state, 48);
            // Unknown directives are forward-compatible; malformed known ones
            // are refused. Both answers are fine, a panic is not.
            let _ = parse_directives(Some(&query));
        }
    }
}
