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
            has_initialization, parse_stream, validate_path,
        },
    },
    domain::{MediaKind, RenditionId, StreamId},
    mux::MediaSegmentFormat,
};

const MULTIVARIANT_NAME: &str = "index.m3u8";
const NAMED_KINDS: [MediaKind; 3] = [MediaKind::Audio, MediaKind::Subtitle, MediaKind::Video];

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
}

impl Resource {
    pub fn rendition(self) -> Option<RenditionId> {
        match self {
            Self::Multivariant => None,
            Self::MediaPlaylist(rendition, _) => Some(rendition),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourcePath {
    pub stream: StreamId,
    pub resource: Resource,
}

/// Parses only HLS manifest paths; shared media is deliberately not routed
/// through this adapter.
pub fn parse_path(path: &str) -> Result<ResourcePath, ResourcePathError> {
    let segment_count = validate_path(path)?;
    let mut tail = path.split('/').filter(|segment| !segment.is_empty()).rev();
    let name = tail.next().ok_or(ResourcePathError::Unrecognized)?;
    let (resource, consumed) = if name == MULTIVARIANT_NAME {
        (Resource::Multivariant, 1)
    } else {
        let rendition = tail.next().ok_or(ResourcePathError::Unrecognized)?;
        let kind = media_playlist_kind(name).ok_or(ResourcePathError::Unrecognized)?;
        let rendition = rendition
            .parse()
            .map(RenditionId)
            .map_err(|_| ResourcePathError::InvalidIdentifier)?;
        (Resource::MediaPlaylist(rendition, kind), 2)
    };
    Ok(ResourcePath {
        stream: parse_stream(path, segment_count.saturating_sub(consumed))?,
        resource,
    })
}

/// The absolute location manifests publish under, or relative names by default.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UriBase(Option<Arc<str>>);

impl UriBase {
    pub fn new(value: impl AsRef<str>) -> Self {
        let trimmed = value.as_ref().trim().trim_end_matches('/');
        Self((!trimmed.is_empty()).then(|| Arc::from(trimmed)))
    }

    pub fn uris(&self, stream: &StreamId) -> PlaylistUris {
        PlaylistUris {
            root: self
                .0
                .as_ref()
                .map(|base| format!("{base}/{}", PercentEncoded(stream.as_str()))),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistUris {
    root: Option<String>,
}

impl PlaylistUris {
    pub fn media_playlist(&self, rendition: RenditionId, kind: MediaKind) -> String {
        let mut out = String::new();
        self.write_prefix(&mut out, None, rendition);
        out.push_str(media_playlist_name(kind));
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
        out
    }

    pub fn has_initialization(&self) -> bool {
        has_initialization(self.format)
    }

    fn media<'a>(&self, resource: MediaResource, out: &'a mut String) -> Option<&'a str> {
        self.uris
            .write_prefix(out, Some(self.rendition), resource.rendition());
        append_media_leaf(out, resource)
    }
}

#[cfg(test)]
mod tests {
    use crate::delivery::uri::{MediaResourcePath, parse_media_path};

    use super::*;

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
    fn manifest_and_media_parsers_have_disjoint_responsibilities() {
        assert_eq!(
            parse_path("/live/camera/3/video.m3u8"),
            Ok(ResourcePath {
                stream: StreamId::new("live/camera"),
                resource: Resource::MediaPlaylist(RenditionId(3), MediaKind::Video),
            })
        );
        assert_eq!(
            parse_path("/live/camera/3/segment/7.m4s"),
            Err(ResourcePathError::Unrecognized)
        );
        assert!(matches!(
            parse_media_path("/live/camera/3/segment/7.m4s"),
            Ok(Some(MediaResourcePath { .. }))
        ));
        assert_eq!(parse_media_path("/live/camera/index.m3u8"), Ok(None));
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
}
