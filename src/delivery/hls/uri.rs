//! What this origin serves: how each resource is named, parsed, and typed.
//!
//! Both directions live here, and so does the `Content-Type` each resource is
//! served under. That is the point of the module: a playlist naming a resource
//! the router cannot parse, or a `.vtt` served as `video/mp4`, are the same
//! class of bug — a name and its meaning drifting apart in two files that never
//! see each other. Here they are one table, read forwards to write a name and
//! backwards to resolve one.
//!
//! [`PlaylistUris`] writes; [`parse_path`] reads; [`Resource::content_type`]
//! says how the result is served.
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
//! sequence numbers. An ID names one immutable object for as long as its stream
//! incarnation lives, which lets a URL outlive the playlist tag that introduced
//! it. HTTP caching remains bounded because a retired stream can later be
//! recreated under the same name and issue those identities again.

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
const MULTIVARIANT_NAME: &str = "index.m3u8";

/// The media type every playlist is served as, whatever it describes.
pub const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

const INITIALIZATION_DIRECTORY: &str = "init";
const SEGMENT_DIRECTORY: &str = "segment";
const PART_DIRECTORY: &str = "part";

/// Every kind a media playlist may be named after.
///
/// Listed so [`media_playlist_name`] and [`media_playlist_kind`] cannot drift:
/// the parser is derived from the speller rather than written alongside it.
const NAMED_KINDS: [MediaKind; 3] = [MediaKind::Audio, MediaKind::Subtitle, MediaKind::Video];

/// Every packaging format a name may spell, for the same reason.
const PACKAGING_FORMATS: [MediaSegmentFormat; 3] = [
    MediaSegmentFormat::Cmaf,
    MediaSegmentFormat::WebVtt,
    MediaSegmentFormat::MpegTs,
];

/// The file name a media playlist of this kind is served as.
const fn media_playlist_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "audio.m3u8",
        MediaKind::Subtitle => "subtitles.m3u8",
        MediaKind::Video => "video.m3u8",
    }
}

/// The kind a file name claims to carry, if it names a media playlist at all.
fn media_playlist_kind(name: &str) -> Option<MediaKind> {
    NAMED_KINDS
        .into_iter()
        .find(|kind| media_playlist_name(*kind) == name)
}

/// One addressable delivery resource within a stream.
///
/// Every variant carries what its name claims about the rendition serving it:
/// a playlist's file name says what kind of media it holds, and a media
/// resource's extension says how it is packaged. Those claims are what make the
/// scheme load-bearing rather than decorative — delivery checks each one once
/// the rendition has been resolved, which is something the router parsing the
/// name cannot do.
///
/// Deliberately does not carry the stream: a stream identity is an arbitrary
/// operator-chosen string that needs percent-encoding and lives in the router's
/// half of the path, while everything here is a fixed word or a number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resource {
    Multivariant,
    MediaPlaylist(RenditionId, MediaKind),
    Initialization(RenditionId, InitializationId, MediaSegmentFormat),
    Segment(RenditionId, SegmentId, MediaSegmentFormat),
    Part(RenditionId, PartId, MediaSegmentFormat),
}

impl Resource {
    /// The rendition this resource belongs to, if it is not stream-wide.
    pub fn rendition(&self) -> Option<RenditionId> {
        match self {
            Self::Multivariant => None,
            Self::MediaPlaylist(rendition, _)
            | Self::Initialization(rendition, _, _)
            | Self::Segment(rendition, _, _)
            | Self::Part(rendition, _, _) => Some(*rendition),
        }
    }

    /// The media type this resource is served under.
    ///
    /// `None` only for a resource its own packaging cannot produce — an
    /// MPEG-TS initialization section — which is also a name that cannot be
    /// written or parsed. The three answers agree because they read one table.
    pub fn content_type(&self) -> Option<&'static str> {
        match self {
            Self::Multivariant | Self::MediaPlaylist(..) => Some(PLAYLIST_CONTENT_TYPE),
            Self::Initialization(_, _, format) => spellings(*format)
                .initialization
                .map(|spelling| spelling.content_type),
            Self::Segment(_, _, format) | Self::Part(_, _, format) => {
                Some(spellings(*format).segment.content_type)
            }
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
    pub fn media_playlist(&self, rendition: RenditionId, kind: MediaKind) -> String {
        self.name(None, Resource::MediaPlaylist(rendition, kind))
            .expect("a media playlist is nameable in every packaging")
    }

    /// Names resources as one rendition's own media playlist writes them.
    pub fn within(&self, rendition: RenditionId, format: MediaSegmentFormat) -> RenditionUris<'_> {
        RenditionUris {
            uris: self,
            rendition,
            format,
        }
    }

    /// Writes one name, from wherever the emitting playlist stands.
    ///
    /// An absolute name is rooted at the stream, so every resource is spelled
    /// the same way regardless of who names it. A relative one is written from
    /// the emitting playlist's own directory: the multivariant playlist sits
    /// above every rendition, a media playlist inside its own.
    fn name(&self, from: Option<RenditionId>, resource: Resource) -> Option<String> {
        let mut out = String::new();
        self.write_name(&mut out, from, resource)?;
        Some(out)
    }

    fn write_name<'a>(
        &self,
        out: &'a mut String,
        from: Option<RenditionId>,
        resource: Resource,
    ) -> Option<&'a str> {
        out.clear();
        let rendition = resource.rendition()?;
        // Writing into a String cannot fail.
        let _ = match (&self.root, from) {
            (Some(root), _) => write!(out, "{root}/{}/", rendition.0),
            (None, None) => write!(out, "{}/", rendition.0),
            (None, Some(emitter)) if emitter != rendition => {
                write!(out, "../{}/", rendition.0)
            }
            (None, Some(_)) => Ok(()),
        };
        match resource {
            // Ruled out by the rendition lookup above.
            Resource::Multivariant => return None,
            Resource::MediaPlaylist(_, kind) => out.push_str(media_playlist_name(kind)),
            Resource::Initialization(_, id, format) => append(
                out,
                INITIALIZATION_DIRECTORY,
                id.0,
                spellings(format).initialization?,
            ),
            Resource::Segment(_, id, format) => {
                append(out, SEGMENT_DIRECTORY, id.0, spellings(format).segment)
            }
            Resource::Part(_, id, format) => {
                append(out, PART_DIRECTORY, id.0, spellings(format).segment)
            }
        }
        Some(out)
    }
}

/// `{directory}/{identifier}.{extension}`: the shape every media resource has.
fn append(out: &mut String, directory: &str, identifier: u64, spelling: Spelling) {
    // Writing into a `String` cannot fail.
    let _ = write!(out, "{directory}/{identifier}.{}", spelling.extension);
}

/// Names the resources of one rendition as its own media playlist must.
///
/// Bound to the rendition and its packaging, so a caller states only which
/// resource it means — and cannot accidentally name one under a sibling's
/// packaging.
#[derive(Clone, Copy, Debug)]
pub struct RenditionUris<'a> {
    uris: &'a PlaylistUris,
    rendition: RenditionId,
    format: MediaSegmentFormat,
}

impl RenditionUris<'_> {
    /// `None` when this packaging has no initialization section to point at.
    pub fn initialization<'a>(&self, id: InitializationId, out: &'a mut String) -> Option<&'a str> {
        self.name(
            Resource::Initialization(self.rendition, id, self.format),
            out,
        )
    }

    pub fn segment<'a>(&self, id: SegmentId, out: &'a mut String) -> &'a str {
        self.expect(Resource::Segment(self.rendition, id, self.format), out)
    }

    pub fn part<'a>(&self, id: PartId, out: &'a mut String) -> &'a str {
        self.expect(Resource::Part(self.rendition, id, self.format), out)
    }

    /// A *sibling's* playlist, which this rendition's packaging says nothing
    /// about — hence the kind rather than a format.
    pub fn sibling_playlist<'a>(
        &self,
        rendition: RenditionId,
        kind: MediaKind,
        out: &'a mut String,
    ) -> &'a str {
        self.expect(Resource::MediaPlaylist(rendition, kind), out)
    }

    /// Whether this packaging has an initialization section at all.
    pub fn has_initialization(&self) -> bool {
        spellings(self.format).initialization.is_some()
    }

    fn name<'a>(&self, resource: Resource, out: &'a mut String) -> Option<&'a str> {
        self.uris.write_name(out, Some(self.rendition), resource)
    }

    fn expect<'a>(&self, resource: Resource, out: &'a mut String) -> &'a str {
        self.name(resource, out)
            .expect("every packaging spells its segments and every playlist")
    }
}

/// How one packaging format spells its resources and serves them.
///
/// Extensions are not cosmetic. Players, caches, and validators all key
/// behaviour off them, and a WebVTT rendition served as `.m4s` is a support
/// question waiting to happen. The media type sits in the same row for the same
/// reason: what a resource is called and what it is served as are one decision,
/// and keeping them in two tables is how they come to disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Spellings {
    initialization: Option<Spelling>,
    segment: Spelling,
}

/// What one role of one format is called, and what it is served as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Spelling {
    extension: &'static str,
    content_type: &'static str,
}

const fn spellings(format: MediaSegmentFormat) -> Spellings {
    match format {
        MediaSegmentFormat::Cmaf => Spellings {
            initialization: Some(Spelling {
                extension: "mp4",
                content_type: "video/mp4",
            }),
            segment: Spelling {
                extension: "m4s",
                content_type: "video/iso.segment",
            },
        },
        // A WebVTT rendition's initialization is its `WEBVTT` header and
        // X-TIMESTAMP-MAP, carried as a real EXT-X-MAP so a segment holds only
        // cues.
        MediaSegmentFormat::WebVtt => Spellings {
            initialization: Some(Spelling {
                extension: "vtt",
                content_type: "text/vtt",
            }),
            segment: Spelling {
                extension: "vtt",
                content_type: "text/vtt",
            },
        },
        // MPEG-TS segments are self-describing, so there is no initialization
        // section to name.
        MediaSegmentFormat::MpegTs => Spellings {
            initialization: None,
            segment: Spelling {
                extension: "ts",
                content_type: "video/mp2t",
            },
        },
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

/// What one request path names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourcePath {
    pub stream: StreamId,
    pub resource: Resource,
}

/// The longest path this origin will even attempt to parse.
///
/// A bound here keeps a pathological path from being split into thousands of
/// segments before anything has decided it is nonsense.
const MAXIMUM_PATH_BYTES: usize = 2_048;
const MAXIMUM_PATH_SEGMENTS: usize = 32;

/// Resolves a request path to the stream and resource it names.
///
/// The inverse of what [`PlaylistUris`] writes, and the reason both live here.
/// The awkward part is that a stream identity is an operator-chosen string that
/// may itself contain slashes — `live/camera` is an ordinary name — so the path
/// cannot be matched left to right. The resource tail is the fixed-width end, so
/// it is parsed from the right and whatever precedes it is the stream.
pub fn parse_path(path: &str) -> Result<ResourcePath, ResourcePathError> {
    if path.len() > MAXIMUM_PATH_BYTES {
        return Err(ResourcePathError::Unrecognized);
    }
    let mut segment_count = 0;
    // Traversal cannot reach anything — every resource is served from memory by
    // identity, not from a filesystem — but a path containing these is not a
    // name this origin ever produced, so it is a miss rather than a lookup.
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        segment_count += 1;
        if segment_count > MAXIMUM_PATH_SEGMENTS || matches!(segment, "." | "..") {
            return Err(ResourcePathError::Unrecognized);
        }
    }

    let (resource, consumed) = parse_tail(path)?;
    let stream_segments = segment_count.saturating_sub(consumed);
    if stream_segments == 0 {
        return Err(ResourcePathError::Unrecognized);
    }
    // The final decoded path can be shorter but never longer in UTF-8 bytes.
    let mut stream = String::with_capacity(path.len());
    for segment in path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .take(stream_segments)
    {
        if !stream.is_empty() {
            stream.push('/');
        }
        stream.push_str(
            percent_decode(segment)
                .ok_or(ResourcePathError::Unrecognized)?
                .as_ref(),
        );
    }
    Ok(ResourcePath {
        stream: StreamId::new(stream),
        resource,
    })
}

/// Parses the rendition-and-resource tail, and how many segments it consumed.
///
/// The extension is part of the name rather than decoration: it is what
/// distinguishes formats sharing one identifier space, so it is resolved to the
/// format that spells it. A name no format spells — `init/1.m4s`, since no
/// packaging calls an initialization section that — is a miss here rather than
/// something delivery has to refuse after a lookup.
fn parse_tail(path: &str) -> Result<(Resource, usize), ResourcePathError> {
    let mut tail = path.split('/').filter(|segment| !segment.is_empty()).rev();
    let name = tail.next().ok_or(ResourcePathError::Unrecognized)?;
    if name == MULTIVARIANT_NAME {
        return Ok((Resource::Multivariant, 1));
    }
    let second = tail.next().ok_or(ResourcePathError::Unrecognized)?;
    if let Some(kind) = media_playlist_kind(name) {
        return Ok((Resource::MediaPlaylist(parse_rendition(second)?, kind), 2));
    }

    let directory = second;
    let rendition = tail.next().ok_or(ResourcePathError::Unrecognized)?;
    let (identifier, extension) = name
        .rsplit_once('.')
        .ok_or(ResourcePathError::Unrecognized)?;
    // Shape first, identifiers second: a path whose directory and extension
    // name nothing is unrecognised, whatever numbers it happens to contain.
    let format = spelled_by(directory, extension).ok_or(ResourcePathError::Unrecognized)?;
    let rendition = parse_rendition(rendition)?;
    let identifier: u64 = identifier
        .parse()
        .map_err(|_| ResourcePathError::InvalidIdentifier)?;
    let resource = match directory {
        INITIALIZATION_DIRECTORY => {
            Resource::Initialization(rendition, InitializationId(identifier), format)
        }
        SEGMENT_DIRECTORY => Resource::Segment(rendition, SegmentId(identifier), format),
        PART_DIRECTORY => Resource::Part(rendition, PartId(identifier), format),
        _ => return Err(ResourcePathError::Unrecognized),
    };
    Ok((resource, 3))
}

/// The format that spells this role with this extension.
///
/// Derived from [`spellings`] rather than written alongside them, so an
/// extension means exactly what the table says it means. The table has to keep
/// each role's extensions unambiguous, which the test
/// `every_spelling_names_exactly_one_format` holds it to.
fn spelled_by(directory: &str, extension: &str) -> Option<MediaSegmentFormat> {
    PACKAGING_FORMATS.into_iter().find(|format| {
        let spellings = spellings(*format);
        match directory {
            INITIALIZATION_DIRECTORY => spellings
                .initialization
                .is_some_and(|spelling| spelling.extension == extension),
            SEGMENT_DIRECTORY | PART_DIRECTORY => spellings.segment.extension == extension,
            _ => false,
        }
    })
}

fn parse_rendition(value: &str) -> Result<RenditionId, ResourcePathError> {
    value
        .parse()
        .map(RenditionId)
        .map_err(|_| ResourcePathError::InvalidIdentifier)
}

/// Percent-decodes one path segment.
///
/// Stream identities are operator-chosen strings that may legitimately contain
/// characters a URL has to escape. Invalid escapes are a decoding failure
/// rather than a pass-through: silently accepting `%zz` would let two spellings
/// name one stream.
fn percent_decode(value: &str) -> Option<Cow<'_, str>> {
    if !value.as_bytes().contains(&b'%') {
        return Some(Cow::Borrowed(value));
    }
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
    decode(value).ok()
}

/// Percent-encodes a hierarchical stream identity for inclusion in a URL path.
struct PercentEncoded<'a>(pub &'a str);

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

    const CMAF: MediaSegmentFormat = MediaSegmentFormat::Cmaf;
    const WEBVTT: MediaSegmentFormat = MediaSegmentFormat::WebVtt;

    /// Relative names, as a playlist served from this origin carries them.
    fn relative() -> PlaylistUris {
        UriBase::default().uris(&StreamId::new("live/camera"))
    }

    #[test]
    fn media_playlists_name_their_own_media_relatively() {
        let uris = relative();
        let names = uris.within(RenditionId(3), CMAF);
        let mut out = String::new();

        assert_eq!(names.segment(SegmentId(7), &mut out), "segment/7.m4s");
        assert_eq!(names.part(PartId(41), &mut out), "part/41.m4s");
        assert_eq!(
            names.initialization(InitializationId(1), &mut out),
            Some("init/1.mp4")
        );
        assert_eq!(
            names.sibling_playlist(RenditionId(9), MediaKind::Audio, &mut out),
            "../9/audio.m3u8",
            "a rendition report steps out of its own directory"
        );
        assert_eq!(
            uris.media_playlist(RenditionId(9), MediaKind::Video),
            "9/video.m3u8",
            "the multivariant playlist already sits above every rendition"
        );
    }

    #[test]
    fn a_configured_base_roots_every_name_at_the_stream() {
        let uris = UriBase::new("https://cdn.example.com/hls/").uris(&StreamId::new("live/a b"));
        let names = uris.within(RenditionId(0), CMAF);
        let mut out = String::new();

        assert_eq!(
            uris.media_playlist(RenditionId(0), MediaKind::Video),
            "https://cdn.example.com/hls/live/a%20b/0/video.m3u8"
        );
        assert_eq!(
            names.segment(SegmentId(7), &mut out),
            "https://cdn.example.com/hls/live/a%20b/0/segment/7.m4s"
        );
        assert_eq!(
            names.sibling_playlist(RenditionId(9), MediaKind::Audio, &mut out),
            "https://cdn.example.com/hls/live/a%20b/9/audio.m3u8",
            "an absolute name is rooted at the stream, so a sibling needs no \
             relative step out"
        );
        assert_eq!(
            UriBase::new("   "),
            UriBase::default(),
            "an empty base is no base at all"
        );
    }

    #[test]
    fn webvtt_names_both_its_header_and_its_cues_as_vtt() {
        let uris = relative();
        let names = uris.within(RenditionId(0), WEBVTT);
        let mut out = String::new();

        assert_eq!(
            names.initialization(InitializationId(1), &mut out),
            Some("init/1.vtt")
        );
        assert_eq!(names.segment(SegmentId(2), &mut out), "segment/2.vtt");
    }

    #[test]
    fn a_format_without_an_initialization_section_cannot_name_or_type_one() {
        let uris = relative();
        let names = uris.within(RenditionId(0), MediaSegmentFormat::MpegTs);
        let mut out = String::new();
        let initialization = Resource::Initialization(
            RenditionId(0),
            InitializationId(1),
            MediaSegmentFormat::MpegTs,
        );

        assert!(!names.has_initialization());
        assert_eq!(names.initialization(InitializationId(1), &mut out), None);
        assert_eq!(initialization.content_type(), None);
        assert_eq!(
            parse_path("/s/0/init/1.ts"),
            Err(ResourcePathError::Unrecognized),
            "naming, typing, and parsing agree because they read one table"
        );
    }

    #[test]
    fn a_resource_is_served_as_its_own_name_says() {
        assert_eq!(
            Resource::Multivariant.content_type(),
            Some(PLAYLIST_CONTENT_TYPE)
        );
        assert_eq!(
            Resource::MediaPlaylist(RenditionId(0), MediaKind::Audio).content_type(),
            Some(PLAYLIST_CONTENT_TYPE)
        );
        assert_eq!(
            Resource::Segment(RenditionId(0), SegmentId(1), CMAF).content_type(),
            Some("video/iso.segment")
        );
        assert_eq!(
            Resource::Part(RenditionId(0), PartId(1), WEBVTT).content_type(),
            Some("text/vtt")
        );
        assert_eq!(
            Resource::Initialization(RenditionId(0), InitializationId(1), CMAF).content_type(),
            Some("video/mp4"),
            "an initialization section is not served as its own segments are"
        );
    }

    #[test]
    fn every_spelling_names_exactly_one_format() {
        for format in PACKAGING_FORMATS {
            let spellings = spellings(format);
            if let Some(initialization) = spellings.initialization {
                assert_eq!(
                    spelled_by(INITIALIZATION_DIRECTORY, initialization.extension),
                    Some(format),
                    "an extension two formats claim would make a name ambiguous"
                );
            }
            for directory in [SEGMENT_DIRECTORY, PART_DIRECTORY] {
                assert_eq!(
                    spelled_by(directory, spellings.segment.extension),
                    Some(format)
                );
            }
        }
    }

    #[test]
    fn a_path_is_parsed_from_the_right_so_a_stream_may_contain_slashes() {
        let parsed = |path| parse_path(path).map(|named| (named.stream, named.resource));

        assert_eq!(
            parsed("/live/camera/index.m3u8"),
            Ok((StreamId::new("live/camera"), Resource::Multivariant))
        );
        assert_eq!(
            parsed("/a/b/c/d/3/subtitles.m3u8"),
            Ok((
                StreamId::new("a/b/c/d"),
                Resource::MediaPlaylist(RenditionId(3), MediaKind::Subtitle)
            ))
        );
        assert_eq!(
            parsed("/live/camera/3/part/41.m4s"),
            Ok((
                StreamId::new("live/camera"),
                Resource::Part(RenditionId(3), PartId(41), CMAF)
            ))
        );
        assert_eq!(
            parsed("/s/3/segment/7.vtt"),
            Ok((
                StreamId::new("s"),
                Resource::Segment(RenditionId(3), SegmentId(7), WEBVTT)
            )),
            "the extension is what tells apart the formats sharing one \
             identifier space"
        );
        assert_eq!(
            parsed("/live%20one/0/video.m3u8").map(|(stream, _)| stream),
            Ok(StreamId::new("live one"))
        );
    }

    #[test]
    fn every_name_a_playlist_emits_is_one_the_router_parses() {
        let uris = relative();
        let rendition = RenditionId(3);
        let names = uris.within(rendition, CMAF);
        let mut out = String::new();

        // Each name paired with the directory the playlist emitting it is
        // served from: the multivariant playlist sits above the renditions, a
        // media playlist inside its own.
        for (emitter, emitted, expected) in [
            (
                "/live/camera/",
                uris.media_playlist(rendition, MediaKind::Audio),
                Resource::MediaPlaylist(rendition, MediaKind::Audio),
            ),
            (
                "/live/camera/3/",
                names
                    .sibling_playlist(RenditionId(9), MediaKind::Video, &mut out)
                    .to_owned(),
                Resource::MediaPlaylist(RenditionId(9), MediaKind::Video),
            ),
            (
                "/live/camera/3/",
                names.segment(SegmentId(7), &mut out).to_owned(),
                Resource::Segment(rendition, SegmentId(7), CMAF),
            ),
            (
                "/live/camera/3/",
                names.part(PartId(41), &mut out).to_owned(),
                Resource::Part(rendition, PartId(41), CMAF),
            ),
            (
                "/live/camera/3/",
                names
                    .initialization(InitializationId(1), &mut out)
                    .expect("a CMAF initialization is nameable")
                    .to_owned(),
                Resource::Initialization(rendition, InitializationId(1), CMAF),
            ),
        ] {
            // Resolved as a client would: against the emitting playlist's own
            // directory, with any relative step applied.
            let path = format!("{emitter}{emitted}").replace("/3/../", "/");
            let parsed = parse_path(&path).expect("the router parses what a playlist emits");
            assert_eq!(parsed.stream, StreamId::new("live/camera"), "{path}");
            assert_eq!(parsed.resource, expected, "{path}");
        }
    }

    #[test]
    fn paths_that_name_nothing_are_refused_rather_than_guessed_at() {
        for path in [
            "/",
            "/index.m3u8",
            "/s/3/video.m3u8x",
            // A media playlist is named after what it carries.
            "/s/3/media.m3u8",
            "/s/3/elsewhere/7.m4s",
            "/s/3/segment/7",
            "/s/3/segment/7.mkv",
            // No packaging calls an initialization section `.m4s`, so the name
            // is a miss before any rendition is looked up.
            "/s/3/init/1.m4s",
            // Traversal reaches nothing, but it is not a name this origin
            // produced either.
            "/s/0/../../etc/passwd",
            // An invalid escape would let two spellings name one stream.
            "/live%zz/0/video.m3u8",
        ] {
            assert_eq!(
                parse_path(path),
                Err(ResourcePathError::Unrecognized),
                "{path}"
            );
        }
        assert_eq!(
            parse_path("/s/3/segment/seven.m4s"),
            Err(ResourcePathError::InvalidIdentifier)
        );
    }

    #[test]
    fn percent_coding_round_trips_and_rejects_invalid_escapes() {
        assert_eq!(
            percent_decode("live/camera").as_deref(),
            Some("live/camera")
        );
        assert_eq!(percent_decode("a%20b").as_deref(), Some("a b"));
        assert_eq!(percent_decode("a%2"), None);
        assert_eq!(percent_decode("a%zz"), None);
        assert_eq!(PercentEncoded("live/a b").to_string(), "live/a%20b");
        assert_eq!(
            PercentEncoded("live/café?#").to_string(),
            "live/caf%C3%A9%3F%23"
        );
        assert_eq!(
            percent_decode("live/caf%C3%A9%3F%23").as_deref(),
            Some("live/café?#")
        );
        assert_eq!(percent_decode("%FF"), None, "stream IDs must be UTF-8");
    }
}
