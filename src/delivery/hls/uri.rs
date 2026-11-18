//! HLS manifest names and the media names those manifests project.
//!
//! Manifest paths belong to HLS. Immutable media paths come from
//! [`delivery::uri`](crate::delivery::uri), so another manifest protocol can
//! reference the same bytes without duplicating the namespace or cache keys.

use std::{fmt::Write, sync::Arc};

use crate::{
    delivery::{
        store::{InitializationId, PartId, SegmentId},
        uri::{
            MediaResource, PercentEncoded, ResourcePathError, append_media_leaf,
            has_initialization, parse_rendition,
        },
    },
    domain::{MediaKind, RenditionId, StreamId},
    mux::MediaSegmentFormat,
};

pub(in crate::delivery) const PLAYLIST_EXTENSION: &str = "m3u8";
const MULTIVARIANT_NAME: &str = "index.m3u8";
const IFRAME_PLAYLIST_NAME: &str = "iframe.m3u8";
const NAMED_KINDS: [MediaKind; 3] = [MediaKind::Audio, MediaKind::Subtitle, MediaKind::Video];
/// Query parameter native HLS substitutes from `EXT-X-DEFINE:QUERYPARAM`.
pub const TOKEN_QUERYPARAM: &str = "token";
/// Suffix appended to every minted URI when playlists carry QUERYPARAM.
const TOKEN_VARIABLE: &str = "token={$token}";
/// Protocol version required by `EXT-X-DEFINE` with `QUERYPARAM`.
pub const QUERYPARAM_VERSION: u8 = 11;

const fn media_playlist_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "audio.m3u8",
        MediaKind::Subtitle => "subtitles.m3u8",
        MediaKind::Video => "video.m3u8",
    }
}

fn media_playlist_kind(name: &str) -> Option<MediaKind> {
    NAMED_KINDS
        .into_iter()
        .find(|kind| media_playlist_name(*kind) == name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resource {
    Multivariant,
    MediaPlaylist(RenditionId, MediaKind),
    IFramePlaylist(RenditionId),
}

impl Resource {
    pub fn rendition(self) -> Option<RenditionId> {
        match self {
            Self::Multivariant => None,
            Self::MediaPlaylist(rendition, _) | Self::IFramePlaylist(rendition) => Some(rendition),
        }
    }
}

/// Resolves a playlist leaf and leaves the stream prefix for the viewer parser.
pub(in crate::delivery) fn parse_manifest<'a, 'path>(
    prefix: &'a [&'path str],
    name: &str,
) -> Result<(&'a [&'path str], Resource), ResourcePathError> {
    if name == MULTIVARIANT_NAME {
        return Ok((prefix, Resource::Multivariant));
    }
    let [stream @ .., rendition] = prefix else {
        return Err(ResourcePathError::Unrecognized);
    };
    // Classify the leaf before parsing its parent so unknown playlist names
    // stay unrecognized regardless of the preceding component.
    let resource = if name == IFRAME_PLAYLIST_NAME {
        Resource::IFramePlaylist(parse_rendition(rendition)?)
    } else {
        let kind = media_playlist_kind(name).ok_or(ResourcePathError::Unrecognized)?;
        Resource::MediaPlaylist(parse_rendition(rendition)?, kind)
    };
    Ok((stream, resource))
}

/// The absolute location manifests publish under, or relative names by default.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UriBase(Option<Arc<str>>);

impl UriBase {
    pub fn new(value: impl AsRef<str>) -> Self {
        let trimmed = value.as_ref().trim().trim_end_matches('/');
        Self((!trimmed.is_empty()).then(|| Arc::from(trimmed)))
    }

    /// `path` under `http.public_url` when one is configured, else unchanged.
    ///
    /// Only the configured base can make a location absolute: a listen
    /// address such as `0.0.0.0:8080` is not reachable, and any host guessed
    /// from it would be wrong behind a proxy or CDN.
    pub fn locate(&self, path: &str) -> String {
        match &self.0 {
            Some(base) => format!("{base}{path}"),
            None => path.to_owned(),
        }
    }

    pub fn uris(&self, stream: &StreamId) -> PlaylistUris {
        PlaylistUris {
            root: self
                .0
                .as_ref()
                .map(|base| format!("{base}/{}", PercentEncoded(stream.as_str()))),
            query_variables: false,
        }
    }
}

/// Server path of a stream's multivariant playlist, e.g. `/live/demo/index.m3u8`.
///
/// Rooted like the media paths lifecycle events carry, so a consumer joins
/// either to the same origin.
pub fn multivariant_path(stream: &StreamId) -> String {
    format!("/{}/{MULTIVARIANT_NAME}", PercentEncoded(stream.as_str()))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistUris {
    root: Option<String>,
    /// When set, every minted URI carries `token={$token}` for native HLS.
    query_variables: bool,
}

impl PlaylistUris {
    /// A second URI set whose names carry the QUERYPARAM substitution.
    #[must_use]
    pub fn with_query_variables(&self) -> Self {
        Self {
            root: self.root.clone(),
            query_variables: true,
        }
    }

    pub fn query_variables(&self) -> bool {
        self.query_variables
    }

    pub fn media_playlist(&self, rendition: RenditionId, kind: MediaKind) -> String {
        let mut out = String::new();
        self.write_prefix(&mut out, None, rendition);
        out.push_str(media_playlist_name(kind));
        self.append_token_variable(&mut out);
        out
    }

    pub fn iframe_playlist(&self, rendition: RenditionId) -> String {
        let mut out = String::new();
        self.write_prefix(&mut out, None, rendition);
        out.push_str(IFRAME_PLAYLIST_NAME);
        self.append_token_variable(&mut out);
        out
    }

    pub fn within(&self, rendition: RenditionId, format: MediaSegmentFormat) -> RenditionUris<'_> {
        RenditionUris {
            uris: self,
            rendition,
            format,
        }
    }

    fn write_prefix(&self, out: &mut String, from: Option<RenditionId>, rendition: RenditionId) {
        out.clear();
        // Writing into a String cannot fail.
        let _ = match (&self.root, from) {
            (Some(root), _) => write!(out, "{root}/{}/", rendition.0),
            (None, None) => write!(out, "{}/", rendition.0),
            (None, Some(emitter)) if emitter != rendition => write!(out, "../{}/", rendition.0),
            (None, Some(_)) => Ok(()),
        };
    }

    fn append_token_variable(&self, out: &mut String) {
        if !self.query_variables {
            return;
        }
        out.push(if out.contains('?') { '&' } else { '?' });
        out.push_str(TOKEN_VARIABLE);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RenditionUris<'a> {
    uris: &'a PlaylistUris,
    rendition: RenditionId,
    format: MediaSegmentFormat,
}

impl RenditionUris<'_> {
    pub fn initialization<'a>(&self, id: InitializationId, out: &'a mut String) -> Option<&'a str> {
        self.media(
            MediaResource::Initialization(self.rendition, id, self.format),
            out,
        )
    }

    pub fn segment<'a>(&self, id: SegmentId, out: &'a mut String) -> &'a str {
        self.media(MediaResource::Segment(self.rendition, id, self.format), out)
            .expect("every packaging spells its segments")
    }

    pub fn part<'a>(&self, id: PartId, out: &'a mut String) -> &'a str {
        self.media(MediaResource::Part(self.rendition, id, self.format), out)
            .expect("every packaging spells its parts")
    }

    pub fn sibling_playlist<'a>(
        &self,
        rendition: RenditionId,
        kind: MediaKind,
        out: &'a mut String,
    ) -> &'a str {
        self.uris.write_prefix(out, Some(self.rendition), rendition);
        out.push_str(media_playlist_name(kind));
        self.uris.append_token_variable(out);
        out
    }

    pub fn has_initialization(&self) -> bool {
        has_initialization(self.format)
    }

    fn media<'a>(&self, resource: MediaResource, out: &'a mut String) -> Option<&'a str> {
        self.uris
            .write_prefix(out, Some(self.rendition), resource.rendition());
        append_media_leaf(out, resource)?;
        self.uris.append_token_variable(out);
        Some(out.as_str())
    }
}

#[cfg(test)]
mod tests {
    use crate::delivery::uri::{ViewerPath, ViewerResource};

    use super::*;

    #[test]
    fn a_playlist_location_is_absolute_only_under_a_configured_base() {
        let path = multivariant_path(&StreamId::new("live/my camera"));
        assert_eq!(path, "/live/my%20camera/index.m3u8");
        assert_eq!(
            ViewerPath::parse(&path),
            Ok(ViewerPath {
                stream: StreamId::new("live/my camera"),
                resource: ViewerResource::Hls(Resource::Multivariant),
            }),
            "the logged path is one the origin serves"
        );
        assert_eq!(UriBase::default().locate(&path), path);
        assert_eq!(
            UriBase::new("https://cdn.example/hls/").locate(&path),
            "https://cdn.example/hls/live/my%20camera/index.m3u8"
        );
    }

    #[test]
    fn relative_and_rooted_names_preserve_the_shared_media_namespace() {
        let stream = StreamId::new("live/a b");
        let relative = UriBase::default().uris(&stream);
        let rooted = UriBase::new("https://cdn.example/hls/").uris(&stream);
        let mut out = String::new();

        assert_eq!(
            relative
                .within(RenditionId(3), MediaSegmentFormat::Cmaf)
                .segment(SegmentId(7), &mut out),
            "segment/7.m4s"
        );
        assert_eq!(
            rooted
                .within(RenditionId(3), MediaSegmentFormat::Cmaf)
                .segment(SegmentId(7), &mut out),
            "https://cdn.example/hls/live/a%20b/3/segment/7.m4s"
        );
        assert_eq!(
            rooted.media_playlist(RenditionId(3), MediaKind::Video),
            "https://cdn.example/hls/live/a%20b/3/video.m3u8"
        );
    }

    #[test]
    fn viewer_parser_recognizes_rendition_playlists() {
        for (name, resource) in [
            (
                "video.m3u8",
                Resource::MediaPlaylist(RenditionId(3), MediaKind::Video),
            ),
            (
                IFRAME_PLAYLIST_NAME,
                Resource::IFramePlaylist(RenditionId(3)),
            ),
        ] {
            assert_eq!(
                ViewerPath::parse(&format!("/live/camera/3/{name}")),
                Ok(ViewerPath {
                    stream: StreamId::new("live/camera"),
                    resource: ViewerResource::Hls(resource),
                })
            );
        }
    }

    #[test]
    fn sibling_playlists_are_relative_to_the_emitter() {
        let uris = UriBase::default().uris(&StreamId::new("live/camera"));
        let rendition = uris.within(RenditionId(3), MediaSegmentFormat::Cmaf);
        let mut out = String::new();
        assert_eq!(
            rendition.sibling_playlist(RenditionId(9), MediaKind::Audio, &mut out),
            "../9/audio.m3u8"
        );
    }

    #[test]
    fn query_variables_suffix_every_minted_name() {
        let uris = UriBase::default()
            .uris(&StreamId::new("live/camera"))
            .with_query_variables();
        let rendition = uris.within(RenditionId(3), MediaSegmentFormat::Cmaf);
        let mut out = String::new();

        assert_eq!(
            uris.media_playlist(RenditionId(3), MediaKind::Video),
            "3/video.m3u8?token={$token}"
        );
        assert_eq!(
            rendition.segment(SegmentId(7), &mut out),
            "segment/7.m4s?token={$token}"
        );
        assert_eq!(
            rendition.sibling_playlist(RenditionId(9), MediaKind::Audio, &mut out),
            "../9/audio.m3u8?token={$token}"
        );
    }
}
