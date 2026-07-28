//! The resource names delivery serves and playlists reference.
//!
//! One definition, shared by the projection that writes these names and the
//! router that resolves them. Keeping them together is the point: a playlist
//! naming a resource the router cannot parse is a class of bug that only shows
//! up as a 404 in a player's network log.
//!
//! ```text
//! {stream}/index.m3u8                 the presentation a player is pointed at
//! {stream}/{rendition}/video.m3u8     one rendition, named after what it carries
//! {stream}/{rendition}/init/1.mp4
//! {stream}/{rendition}/segment/7.m4s
//! {stream}/{rendition}/part/41.m4s
//! ```
//!
//! A playlist's file name states what it is, so a URL in a network trace or an
//! access log identifies itself without the reader having to resolve a
//! rendition number first. The number is still what keeps two renditions of one
//! kind apart, which is why it stays in the path as the directory.
//!
//! Projection keeps every name **relative to the playlist that emits it** by
//! default, so nothing here need know a host, a scheme, or a deployment path
//! prefix. A media playlist at `.../{rendition}/video.m3u8` names its own media
//! as `segment/7.m4s` and a sibling as `../3/audio.m3u8`. [`UriBase`] roots them
//! at a configured origin instead, for the deployments where a relative name
//! does not survive the trip.
//!
//! Resource names are built from durable store identities rather than media
//! sequence numbers. An ID names one immutable object for as long as it is
//! fetchable, which is what makes segment and part responses cacheable forever
//! and what lets a URL outlive the playlist tag that introduced it.

use std::{
    borrow::Cow,
    fmt::{self, Display, Write},
    sync::Arc,
};

use derive_more::Display;
use urlencoding::{Encoded, decode};

use crate::{
    delivery::hls::{InitializationId, PartId, SegmentId},
    domain::{MediaKind, RenditionId, StreamId},
    mux::MediaSegmentFormat,
};

/// The file name the multivariant playlist is served as.
///
/// `index` rather than `master`: it is the entry point a player is handed,
/// which is what the name should say.
pub const MULTIVARIANT_NAME: &str = "index.m3u8";

const INITIALIZATION_DIRECTORY: &str = "init";
const SEGMENT_DIRECTORY: &str = "segment";
const PART_DIRECTORY: &str = "part";

/// Every kind a media playlist may be named after.
///
/// Listed so [`media_playlist_name`] and [`media_playlist_kind`] cannot drift:
/// the parser is derived from the speller rather than written alongside it.
const NAMED_KINDS: [MediaKind; 3] = [MediaKind::Audio, MediaKind::Subtitle, MediaKind::Video];

/// The file name a media playlist of this kind is served as.
pub const fn media_playlist_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "audio.m3u8",
        MediaKind::Subtitle => "subtitles.m3u8",
        MediaKind::Video => "video.m3u8",
    }
}

/// The kind a file name claims to carry, if it names a media playlist at all.
pub fn media_playlist_kind(name: &str) -> Option<MediaKind> {
    NAMED_KINDS
        .into_iter()
        .find(|kind| media_playlist_name(*kind) == name)
}

/// One addressable delivery resource within a stream.
///
/// Deliberately does not carry the stream: a stream identity is an arbitrary
/// operator-chosen string that needs percent-encoding and lives in the router's
/// half of the path, while everything here is a fixed word or a number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resource {
    Multivariant,
    /// A rendition's media playlist, and the kind its name claims.
    ///
    /// The kind is part of the name, so it is parsed with the rest of it and
    /// checked once the rendition has been resolved. A playlist served under a
    /// name that misdescribes it would make the whole scheme decorative.
    MediaPlaylist(RenditionId, MediaKind),
    Initialization(RenditionId, InitializationId),
    Segment(RenditionId, SegmentId),
    Part(RenditionId, PartId),
}

impl Resource {
    /// The rendition this resource belongs to, if it is not stream-wide.
    pub fn rendition(&self) -> Option<RenditionId> {
        match self {
            Self::Multivariant => None,
            Self::MediaPlaylist(rendition, _)
            | Self::Initialization(rendition, _)
            | Self::Segment(rendition, _)
            | Self::Part(rendition, _) => Some(*rendition),
        }
    }
}

/// The absolute location this origin's resources are published under.
///
/// Unset by default, which leaves every name relative to the playlist that
/// emits it: the same bytes are then correct behind any host, port, or path
/// prefix, and a rendered playlist stays reusable no matter how a request
/// arrived. Set — to `https://cdn.example.com/hls`, or to a bare `/hls` — every
/// name a playlist emits is rooted there instead. That is what a deployment
/// needs when a relative name cannot survive the trip: playlists handed to a
/// player by something other than a fetch of this origin, or media served from
/// a different host than the playlists naming it.
///
/// Configuration rather than a request-derived value (a `Host` header, say)
/// deliberately. Playlist rendering is shared between every viewer of a stream,
/// and a projection that varied per request would have to be rendered per
/// request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UriBase(Option<Arc<str>>);

impl UriBase {
    /// Builds a base from an operator-configured value.
    ///
    /// Trailing slashes are dropped and an empty value is the same as none, so
    /// joining below is one rule rather than three.
    pub fn new(value: impl AsRef<str>) -> Self {
        let trimmed = value.as_ref().trim().trim_end_matches('/');
        Self((!trimmed.is_empty()).then(|| Arc::from(trimmed)))
    }

    pub fn is_absolute(&self) -> bool {
        self.0.is_some()
    }

    /// How the playlists of one stream name what they reference.
    pub fn uris(&self, stream: &StreamId) -> PlaylistUris {
        PlaylistUris {
            root: self
                .0
                .as_ref()
                .map(|base| format!("{base}/{}", PercentEncoded(stream.as_str()))),
        }
    }
}

/// Names resources as the playlists of one stream reference them.
///
/// Relative by default; rooted at [`UriBase`] and the stream's own path when one
/// is configured.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaylistUris {
    /// Everything preceding a resource's own path, when names are absolute.
    root: Option<String>,
}

impl PlaylistUris {
    /// Names a rendition's media playlist as the multivariant playlist does.
    ///
    /// The multivariant playlist sits directly above every rendition directory,
    /// so a relative name here is just the directory and the file.
    pub fn in_multivariant(&self, rendition: RenditionId, kind: MediaKind) -> String {
        let name = media_playlist_name(kind);
        match &self.root {
            Some(root) => format!("{root}/{}/{name}", rendition.0),
            None => format!("{}/{name}", rendition.0),
        }
    }

    /// Names one resource as a rendition's own media playlist does.
    ///
    /// `None` for a resource that cannot be named from there: a format without
    /// an initialization section has none to point at, and the multivariant
    /// playlist sits above every rendition directory, so it is not something a
    /// media playlist has occasion to name.
    pub fn in_media_playlist(&self, naming: ResourceNaming, resource: Resource) -> Option<String> {
        let rendition = resource.rendition()?;
        // An absolute name is rooted at the stream, so a rendition's own media
        // and a sibling's playlist are spelled the same way. A relative one is
        // written from inside the rendition's own directory, which is what
        // keeps it free of any prefix.
        let mut out = match (&self.root, resource) {
            (Some(root), _) => format!("{root}/{}/", rendition.0),
            (None, Resource::MediaPlaylist(..)) => format!("../{}/", rendition.0),
            (None, _) => String::new(),
        };
        match resource {
            Resource::Multivariant => return None,
            Resource::MediaPlaylist(_, kind) => out.push_str(media_playlist_name(kind)),
            Resource::Initialization(_, initialization) => append(
                &mut out,
                INITIALIZATION_DIRECTORY,
                initialization.0,
                naming.initialization?,
            ),
            Resource::Segment(_, segment) => {
                append(&mut out, SEGMENT_DIRECTORY, segment.0, naming.segment)
            }
            Resource::Part(_, part) => append(&mut out, PART_DIRECTORY, part.0, naming.segment),
        }
        Some(out)
    }
}

/// `{directory}/{identifier}.{extension}`: the shape every media resource has.
fn append(out: &mut String, directory: &str, identifier: u64, extension: &str) {
    // Writing into a `String` cannot fail.
    let _ = write!(out, "{directory}/{identifier}.{extension}");
}

/// How one packaging format spells its resources.
///
/// Extensions are not cosmetic. Players, caches, and validators all key
/// behaviour off them, and a WebVTT rendition served as `.m4s` is a support
/// question waiting to happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceNaming {
    initialization: Option<&'static str>,
    segment: &'static str,
}

impl ResourceNaming {
    pub fn for_format(format: MediaSegmentFormat) -> Self {
        match format {
            MediaSegmentFormat::Cmaf => Self {
                initialization: Some("mp4"),
                segment: "m4s",
            },
            // A WebVTT rendition's initialization is its `WEBVTT` header and
            // X-TIMESTAMP-MAP, carried as a real EXT-X-MAP so a segment holds
            // only cues.
            MediaSegmentFormat::WebVtt => Self {
                initialization: Some("vtt"),
                segment: "vtt",
            },
            // MPEG-TS segments are self-describing, so there is no
            // initialization section to name.
            MediaSegmentFormat::MpegTs => Self {
                initialization: None,
                segment: "ts",
            },
        }
    }

    /// Whether this format has an initialization section at all.
    pub fn has_initialization(&self) -> bool {
        self.initialization.is_some()
    }
}

/// Why a request path does not name a resource this origin serves.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
pub enum ResourcePathError {
    #[display("the path does not name a delivery resource")]
    Unrecognized,
    #[display("the path names a resource with an unusable identifier")]
    InvalidIdentifier,
}

/// Parses the rendition-and-resource tail of a request path.
///
/// Takes the path already split into segments, and returns how many trailing
/// segments the resource consumed so the caller can attribute the rest to the
/// stream. Parsing from the right is deliberate: a stream identity may itself
/// contain slashes, so the resource tail is the only fixed-width end of the
/// path.
///
/// Extensions are checked rather than ignored, so a request for `segment/7.mp4`
/// on a WebVTT rendition is a miss rather than a silently different resource.
pub fn parse_tail(segments: &[&str]) -> Result<(Resource, usize), ResourcePathError> {
    if let [.., name] = segments
        && *name == MULTIVARIANT_NAME
    {
        return Ok((Resource::Multivariant, 1));
    }
    if let [.., rendition, name] = segments
        && let Some(kind) = media_playlist_kind(name)
    {
        return Ok((
            Resource::MediaPlaylist(parse_rendition(rendition)?, kind),
            2,
        ));
    }

    let [.., rendition, directory, file] = segments else {
        return Err(ResourcePathError::Unrecognized);
    };
    let (identifier, extension) = file
        .rsplit_once('.')
        .ok_or(ResourcePathError::Unrecognized)?;
    let rendition = parse_rendition(rendition)?;
    let identifier: u64 = identifier
        .parse()
        .map_err(|_| ResourcePathError::InvalidIdentifier)?;
    let resource = match *directory {
        INITIALIZATION_DIRECTORY => {
            Resource::Initialization(rendition, InitializationId(identifier))
        }
        SEGMENT_DIRECTORY => Resource::Segment(rendition, SegmentId(identifier)),
        PART_DIRECTORY => Resource::Part(rendition, PartId(identifier)),
        _ => return Err(ResourcePathError::Unrecognized),
    };
    // The extension is part of the name, not decoration: it is what
    // distinguishes formats sharing one identifier space.
    if extension.is_empty() {
        return Err(ResourcePathError::Unrecognized);
    }
    Ok((resource, 3))
}

fn parse_rendition(value: &str) -> Result<RenditionId, ResourcePathError> {
    value
        .parse()
        .map(RenditionId)
        .map_err(|_| ResourcePathError::InvalidIdentifier)
}

/// Whether a parsed resource's extension matches the format serving it.
///
/// Separate from [`parse_tail`] because the format is only known once the
/// rendition has been looked up, which the router cannot do while parsing.
pub fn extension_matches(naming: ResourceNaming, resource: Resource, path: &str) -> bool {
    let Some(extension) = path.rsplit_once('.').map(|(_, extension)| extension) else {
        return false;
    };
    match resource {
        Resource::Multivariant | Resource::MediaPlaylist(..) => true,
        Resource::Initialization(..) => naming.initialization == Some(extension),
        Resource::Segment(..) | Resource::Part(..) => naming.segment == extension,
    }
}

/// Percent-decodes one path segment.
///
/// Stream identities are operator-chosen strings that may legitimately contain
/// characters a URL has to escape. Invalid escapes are a decoding failure
/// rather than a pass-through: silently accepting `%zz` would let two spellings
/// name one stream.
pub fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if !bytes.get(index + 1)?.is_ascii_hexdigit()
                || !bytes.get(index + 2)?.is_ascii_hexdigit()
            {
                return None;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    decode(value).ok().map(Cow::into_owned)
}

/// Percent-encodes a hierarchical stream identity for inclusion in a URL path.
pub struct PercentEncoded<'a>(pub &'a str);

impl Display for PercentEncoded<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, component) in self.0.split('/').enumerate() {
            if index > 0 {
                formatter.write_char('/')?;
            }
            Encoded::str(component).fmt(formatter)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmaf() -> ResourceNaming {
        ResourceNaming::for_format(MediaSegmentFormat::Cmaf)
    }

    fn relative() -> PlaylistUris {
        UriBase::default().uris(&StreamId::new("live/camera"))
    }

    #[test]
    fn media_playlists_name_their_own_media_relatively() {
        let uris = relative();
        let rendition = RenditionId(3);

        assert_eq!(
            uris.in_media_playlist(cmaf(), Resource::Segment(rendition, SegmentId(7))),
            Some("segment/7.m4s".to_owned())
        );
        assert_eq!(
            uris.in_media_playlist(cmaf(), Resource::Part(rendition, PartId(41))),
            Some("part/41.m4s".to_owned())
        );
        assert_eq!(
            uris.in_media_playlist(
                cmaf(),
                Resource::Initialization(rendition, InitializationId(1))
            ),
            Some("init/1.mp4".to_owned())
        );
        assert_eq!(
            uris.in_media_playlist(
                cmaf(),
                Resource::MediaPlaylist(RenditionId(9), MediaKind::Audio)
            ),
            Some("../9/audio.m3u8".to_owned()),
            "a rendition report steps out of its own directory"
        );
        assert_eq!(
            uris.in_multivariant(RenditionId(9), MediaKind::Video),
            "9/video.m3u8",
            "the multivariant playlist already sits above every rendition"
        );
    }

    #[test]
    fn a_configured_base_roots_every_name_at_the_stream() {
        let uris = UriBase::new("https://cdn.example.com/hls/").uris(&StreamId::new("live/a b"));

        assert_eq!(
            uris.in_multivariant(RenditionId(0), MediaKind::Video),
            "https://cdn.example.com/hls/live/a%20b/0/video.m3u8"
        );
        assert_eq!(
            uris.in_media_playlist(cmaf(), Resource::Segment(RenditionId(0), SegmentId(7))),
            Some("https://cdn.example.com/hls/live/a%20b/0/segment/7.m4s".to_owned())
        );
        assert_eq!(
            uris.in_media_playlist(
                cmaf(),
                Resource::MediaPlaylist(RenditionId(9), MediaKind::Audio)
            ),
            Some("https://cdn.example.com/hls/live/a%20b/9/audio.m3u8".to_owned()),
            "an absolute name is rooted at the stream, so a sibling needs no \
             relative step out"
        );
        assert!(!UriBase::new("   ").is_absolute(), "an empty base is none");
    }

    #[test]
    fn webvtt_names_both_its_header_and_its_cues_as_vtt() {
        let uris = relative();
        let naming = ResourceNaming::for_format(MediaSegmentFormat::WebVtt);
        let rendition = RenditionId(0);

        assert_eq!(
            uris.in_media_playlist(
                naming,
                Resource::Initialization(rendition, InitializationId(1))
            ),
            Some("init/1.vtt".to_owned())
        );
        assert_eq!(
            uris.in_media_playlist(naming, Resource::Segment(rendition, SegmentId(2))),
            Some("segment/2.vtt".to_owned())
        );
    }

    #[test]
    fn a_format_without_an_initialization_section_cannot_name_one() {
        let naming = ResourceNaming::for_format(MediaSegmentFormat::MpegTs);

        assert!(!naming.has_initialization());
        assert_eq!(
            relative().in_media_playlist(
                naming,
                Resource::Initialization(RenditionId(0), InitializationId(1))
            ),
            None
        );
    }

    #[test]
    fn paths_are_parsed_from_the_right_so_a_stream_may_contain_slashes() {
        assert_eq!(
            parse_tail(&["live", "camera", "index.m3u8"]),
            Ok((Resource::Multivariant, 1))
        );
        assert_eq!(
            parse_tail(&["live", "camera", "3", "video.m3u8"]),
            Ok((Resource::MediaPlaylist(RenditionId(3), MediaKind::Video), 2))
        );
        assert_eq!(
            parse_tail(&["live", "camera", "3", "subtitles.m3u8"]),
            Ok((
                Resource::MediaPlaylist(RenditionId(3), MediaKind::Subtitle),
                2
            ))
        );
        assert_eq!(
            parse_tail(&["live", "camera", "3", "part", "41.m4s"]),
            Ok((Resource::Part(RenditionId(3), PartId(41)), 3))
        );
        assert_eq!(
            parse_tail(&["a", "3", "segment", "7.vtt"]),
            Ok((Resource::Segment(RenditionId(3), SegmentId(7)), 3))
        );
    }

    #[test]
    fn every_name_a_playlist_emits_is_one_the_router_parses() {
        let uris = relative();
        let rendition = RenditionId(3);

        for (emitted, expected) in [
            (
                uris.in_multivariant(rendition, MediaKind::Audio),
                Resource::MediaPlaylist(rendition, MediaKind::Audio),
            ),
            (
                uris.in_media_playlist(cmaf(), Resource::Segment(rendition, SegmentId(7)))
                    .expect("a CMAF segment is nameable"),
                Resource::Segment(rendition, SegmentId(7)),
            ),
            (
                uris.in_media_playlist(cmaf(), Resource::Part(rendition, PartId(41)))
                    .expect("a CMAF part is nameable"),
                Resource::Part(rendition, PartId(41)),
            ),
            (
                uris.in_media_playlist(
                    cmaf(),
                    Resource::Initialization(rendition, InitializationId(1)),
                )
                .expect("a CMAF initialization is nameable"),
                Resource::Initialization(rendition, InitializationId(1)),
            ),
        ] {
            // As the router sees it: the stream's own segments, then whatever
            // the playlist named, with the relative step already resolved.
            let mut segments = vec!["live", "camera", "3"];
            segments.extend(emitted.trim_start_matches("../3/").split('/'));
            let parsed = parse_tail(&segments).expect("the router parses what a playlist emits");
            assert_eq!(parsed.0, expected, "{emitted}");
        }
    }

    #[test]
    fn unparseable_paths_are_refused_rather_than_guessed_at() {
        assert_eq!(
            parse_tail(&["3", "video.m3u8x"]),
            Err(ResourcePathError::Unrecognized)
        );
        assert_eq!(
            parse_tail(&["3", "media.m3u8"]),
            Err(ResourcePathError::Unrecognized),
            "a media playlist is named after what it carries"
        );
        assert_eq!(
            parse_tail(&["3", "segment", "seven.m4s"]),
            Err(ResourcePathError::InvalidIdentifier)
        );
        assert_eq!(
            parse_tail(&["3", "elsewhere", "7.m4s"]),
            Err(ResourcePathError::Unrecognized)
        );
        assert_eq!(
            parse_tail(&["index.m3u8x"]),
            Err(ResourcePathError::Unrecognized)
        );
    }

    #[test]
    fn an_extension_belonging_to_another_format_is_a_miss() {
        let resource = Resource::Segment(RenditionId(0), SegmentId(7));

        assert!(extension_matches(cmaf(), resource, "segment/7.m4s"));
        assert!(
            !extension_matches(cmaf(), resource, "segment/7.vtt"),
            "one identifier space is shared by every format, so the extension \
             is what tells them apart"
        );
    }

    #[test]
    fn percent_coding_round_trips_and_rejects_invalid_escapes() {
        assert_eq!(
            percent_decode("live/camera"),
            Some("live/camera".to_owned())
        );
        assert_eq!(percent_decode("a%20b"), Some("a b".to_owned()));
        assert_eq!(percent_decode("a%2"), None);
        assert_eq!(percent_decode("a%zz"), None);
        assert_eq!(PercentEncoded("live/a b").to_string(), "live/a%20b");
        assert_eq!(
            PercentEncoded("live/café?#").to_string(),
            "live/caf%C3%A9%3F%23"
        );
        assert_eq!(
            percent_decode("live/caf%C3%A9%3F%23"),
            Some("live/café?#".to_owned())
        );
        assert_eq!(percent_decode("%FF"), None, "stream IDs must be UTF-8");
    }
}
