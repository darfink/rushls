//! Stable names for retained media, shared by every manifest protocol.
//!
//! Manifest adapters own their own entry-point and playlist names. The larger
//! immutable objects do not: HLS and DASH should point at the same
//! `{stream}/{rendition}/segment/7.m4s` and therefore share CDN cache entries.
//! Naming, parsing, and content type are derived from one packaging table so a
//! `.vtt` can never be routed as an MP4 fragment.

use std::{
    borrow::Cow,
    fmt::{self, Display, Write},
};

use derive_more::Display;
use urlencoding::{Encoded, decode};

use crate::{
    delivery::{
        hls::uri as hls,
        store::{InitializationId, PartId, SegmentId},
    },
    domain::{MediaKind, RenditionId, StreamId},
    mux::MediaSegmentFormat,
};

const INITIALIZATION_DIRECTORY: &str = "init";
const SEGMENT_DIRECTORY: &str = "segment";
const PART_DIRECTORY: &str = "part";
const PACKAGING_FORMATS: [MediaSegmentFormat; 3] = [
    MediaSegmentFormat::Cmaf,
    MediaSegmentFormat::WebVtt,
    MediaSegmentFormat::MpegTs,
];

/// A media type the delivery surface can emit.
///
/// One variant per media type, each naming its own spelling in full. The
/// ISO-BMFF ones are split by what the track carries because that is what the
/// media type says: an audio-only segment and a video one are framed
/// identically, so nothing about the bytes distinguishes them and answering
/// \`video/...\` for the first is simply wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentType {
    /// Any ISO-BMFF object carrying audio, initialization or segment alike.
    ///
    /// Video distinguishes the two and audio does not, which looks like an
    /// oversight and is not: Apple's authoring specification gives
    /// \`video/iso.segment\` for a video media segment but \`audio/mp4\` for both
    /// audio roles, and \`mediastreamvalidator\` enforces it — serving the
    /// symmetric \`audio/iso.segment\` is reported as the wrong MIME type. The
    /// asymmetry is Apple's; recording it here is what keeps it from being
    /// re-derived, and wrongly, the next time someone tidies this table.
    AudioMp4,
    /// An ISO-BMFF initialization object for a video track.
    VideoMp4,
    /// An ISO-BMFF media segment carrying video.
    VideoSegment,
    MpegTs,
    WebVtt,
    /// Text emitted by a manifest adapter, with its protocol media type.
    Manifest(&'static str),
}

impl ContentType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::AudioMp4 => "audio/mp4",
            Self::VideoMp4 => "video/mp4",
            Self::VideoSegment => "video/iso.segment",
            Self::MpegTs => "video/mp2t",
            Self::WebVtt => "text/vtt",
            Self::Manifest(name) => name,
        }
    }

    pub const fn is_text(self) -> bool {
        matches!(self, Self::WebVtt | Self::Manifest(_))
    }
}

/// One retained media object, independent of the manifest protocol naming it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaResource {
    Initialization(RenditionId, InitializationId, MediaSegmentFormat),
    Segment(RenditionId, SegmentId, MediaSegmentFormat),
    Part(RenditionId, PartId, MediaSegmentFormat),
}

impl MediaResource {
    pub fn rendition(self) -> RenditionId {
        match self {
            Self::Initialization(rendition, ..)
            | Self::Segment(rendition, ..)
            | Self::Part(rendition, ..) => rendition,
        }
    }

    pub const fn format(self) -> MediaSegmentFormat {
        match self {
            Self::Initialization(_, _, format)
            | Self::Segment(_, _, format)
            | Self::Part(_, _, format) => format,
        }
    }

    /// The media type this object is served as.
    ///
    /// \`kind\` is required rather than inferred because a [\`MediaResource\`] is
    /// parsed from a request path, which names a rendition but says nothing
    /// about what that rendition carries. Only the store knows, so only a
    /// caller holding its snapshot can answer.
    ///
    /// \`None\` when this combination has no media type: a packaging with no
    /// initialization object, or a subtitle track in ISO-BMFF, which has no
    /// spelling here because nothing produces one.
    pub const fn content_type(self, kind: MediaKind) -> Option<ContentType> {
        match self {
            Self::Initialization(_, _, format) => match spellings(format).initialization {
                Some(spelling) => spelling.content_type.of(kind),
                None => None,
            },
            Self::Segment(_, _, format) | Self::Part(_, _, format) => {
                spellings(format).segment.content_type.of(kind)
            }
        }
    }

    /// Whether this object has a gzip representation.
    ///
    /// A property of the packaging alone: whether bytes are text does not
    /// depend on whether the track they describe is audio or video.
    pub const fn compressible(self) -> bool {
        is_text(self.format())
    }
}

/// Whether media in this packaging is text and may have a gzip representation.
pub const fn is_text(format: MediaSegmentFormat) -> bool {
    spellings(format).segment.content_type.is_text()
}

pub(crate) const fn has_initialization(format: MediaSegmentFormat) -> bool {
    spellings(format).initialization.is_some()
}

#[derive(Clone, Copy)]
struct Spellings {
    initialization: Option<Spelling>,
    segment: Spelling,
}

#[derive(Clone, Copy)]
struct Spelling {
    extension: &'static str,
    content_type: Typed,
}

/// Whether one object's media type depends on what its track carries.
///
/// Making the dependency a property of the table entry is the point: reading
/// it says which containers care and which do not, instead of leaving that in
/// a fixup applied somewhere else to a value that was already wrong.
#[derive(Clone, Copy)]
enum Typed {
    /// The same media type whatever the track carries.
    Fixed(ContentType),
    /// ISO-BMFF, whose media type names the track's media.
    ByMedia {
        audio: ContentType,
        video: ContentType,
    },
}

impl Typed {
    /// The media type for a track of \`kind\`, or \`None\` if there is not one.
    ///
    /// Subtitles in ISO-BMFF are the \`None\`. This origin packages every
    /// subtitle rendition as WebVTT — the CMAF packager refuses a subtitle
    /// track outright — so the combination cannot arise today, and answering
    /// it honestly costs nothing.
    ///
    /// What it buys is that the honest answer is the *only* one available.
    /// Folding subtitles in with video to keep this total would compile, never
    /// fire, and then serve \`video/iso.segment\` on WebVTT bytes the day
    /// somebody adds \`wvtt\` or \`stpp\` output. A \`None\` becomes a request
    /// error that names the resource; a wrong \`Content-Type\` becomes a player
    /// bug reported weeks later.
    ///
    /// It is deliberately not a panic. This runs on the request path for every
    /// media object, subtitle segments included, so an \`unreachable!()\` here
    /// would be reachable by anyone who can issue a GET.
    const fn of(self, kind: MediaKind) -> Option<ContentType> {
        match (self, kind) {
            (Self::Fixed(content_type), _) => Some(content_type),
            (Self::ByMedia { audio, .. }, MediaKind::Audio) => Some(audio),
            (Self::ByMedia { video, .. }, MediaKind::Video) => Some(video),
            (Self::ByMedia { .. }, MediaKind::Subtitle) => None,
        }
    }

    /// Only a fixed media type is ever text; no ISO-BMFF object is.
    const fn is_text(self) -> bool {
        match self {
            Self::Fixed(content_type) => content_type.is_text(),
            Self::ByMedia { .. } => false,
        }
    }
}

const fn spellings(format: MediaSegmentFormat) -> Spellings {
    match format {
        MediaSegmentFormat::Cmaf => Spellings {
            initialization: Some(Spelling {
                extension: "mp4",
                content_type: Typed::ByMedia {
                    audio: ContentType::AudioMp4,
                    video: ContentType::VideoMp4,
                },
            }),
            segment: Spelling {
                extension: "m4s",
                content_type: Typed::ByMedia {
                    audio: ContentType::AudioMp4,
                    video: ContentType::VideoSegment,
                },
            },
        },
        // WebVTT's header is a real initialization object carried by EXT-X-MAP.
        MediaSegmentFormat::WebVtt => Spellings {
            initialization: Some(Spelling {
                extension: "vtt",
                content_type: Typed::Fixed(ContentType::WebVtt),
            }),
            segment: Spelling {
                extension: "vtt",
                content_type: Typed::Fixed(ContentType::WebVtt),
            },
        },
        // MPEG-TS segments are self-describing.
        MediaSegmentFormat::MpegTs => Spellings {
            initialization: None,
            segment: Spelling {
                extension: "ts",
                content_type: Typed::Fixed(ContentType::MpegTs),
            },
        },
    }
}

/// Appends the portion below a rendition directory.
///
/// Restricted visibility is intentional: manifest adapters need the shared
/// spelling table, while callers should deal in typed resources instead.
pub(crate) fn append_media_leaf(out: &mut String, resource: MediaResource) -> Option<&str> {
    let (directory, identifier, spelling) = match resource {
        MediaResource::Initialization(_, id, format) => (
            INITIALIZATION_DIRECTORY,
            id.0,
            spellings(format).initialization?,
        ),
        MediaResource::Segment(_, id, format) => {
            (SEGMENT_DIRECTORY, id.0, spellings(format).segment)
        }
        MediaResource::Part(_, id, format) => (PART_DIRECTORY, id.0, spellings(format).segment),
    };
    let _ = write!(out, "{directory}/{identifier}.{}", spelling.extension);
    Some(out)
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
pub enum ResourcePathError {
    #[display("the path does not name a delivery resource")]
    Unrecognized,
    #[display("the path names a resource with an unusable identifier")]
    InvalidIdentifier,
}

/// The viewer namespace resolved once for authorization, attribution and delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewerPath {
    pub stream: StreamId,
    pub resource: ViewerResource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewerResource {
    Media(MediaResource),
    Hls(hls::Resource),
}

const MAXIMUM_PATH_BYTES: usize = 2_048;
const MAXIMUM_PATH_SEGMENTS: usize = 32;

impl ViewerPath {
    pub fn parse(path: &str) -> Result<Self, ResourcePathError> {
        if path.len() > MAXIMUM_PATH_BYTES {
            return Err(ResourcePathError::Unrecognized);
        }
        // Bound the component list without allocating. Empty components have
        // always been ignored, including a trailing slash after the leaf.
        let mut segments = [""; MAXIMUM_PATH_SEGMENTS];
        let mut count = 0;
        for segment in path.split('/').filter(|segment| !segment.is_empty()) {
            if matches!(segment, "." | "..") {
                return Err(ResourcePathError::Unrecognized);
            }
            *segments
                .get_mut(count)
                .ok_or(ResourcePathError::Unrecognized)? = segment;
            count += 1;
        }
        let [prefix @ .., name] = &segments[..count] else {
            return Err(ResourcePathError::Unrecognized);
        };
        let (identifier, extension) = name
            .rsplit_once('.')
            .ok_or(ResourcePathError::Unrecognized)?;

        // Select the namespace by the leaf. A stream component named
        // `segment`, `part` or `init` can still lead to an HLS manifest.
        let (stream, resource) = if extension == hls::PLAYLIST_EXTENSION {
            let (stream, resource) = hls::parse_manifest(prefix, name)?;
            (stream, ViewerResource::Hls(resource))
        } else {
            let [stream @ .., rendition, directory] = prefix else {
                return Err(ResourcePathError::Unrecognized);
            };
            let resource = parse_media_leaf(rendition, directory, identifier, extension)?;
            (stream, ViewerResource::Media(resource))
        };
        Ok(Self {
            stream: parse_stream(stream)?,
            resource,
        })
    }
}

fn parse_media_leaf(
    rendition: &str,
    directory: &str,
    identifier: &str,
    extension: &str,
) -> Result<MediaResource, ResourcePathError> {
    let format = spelled_by(directory, extension).ok_or(ResourcePathError::Unrecognized)?;
    let rendition = parse_rendition(rendition)?;
    let identifier = identifier
        .parse::<u64>()
        .map_err(|_| ResourcePathError::InvalidIdentifier)?;
    Ok(match directory {
        INITIALIZATION_DIRECTORY => {
            MediaResource::Initialization(rendition, InitializationId(identifier), format)
        }
        SEGMENT_DIRECTORY => MediaResource::Segment(rendition, SegmentId(identifier), format),
        PART_DIRECTORY => MediaResource::Part(rendition, PartId(identifier), format),
        _ => unreachable!("the directory has a packaging spelling"),
    })
}

fn spelled_by(directory: &str, extension: &str) -> Option<MediaSegmentFormat> {
    PACKAGING_FORMATS.into_iter().find(|format| {
        let spelling = spellings(*format);
        match directory {
            INITIALIZATION_DIRECTORY => spelling
                .initialization
                .is_some_and(|value| value.extension == extension),
            SEGMENT_DIRECTORY | PART_DIRECTORY => spelling.segment.extension == extension,
            _ => false,
        }
    })
}

pub(in crate::delivery) fn parse_rendition(value: &str) -> Result<RenditionId, ResourcePathError> {
    value
        .parse()
        .map(RenditionId)
        .map_err(|_| ResourcePathError::InvalidIdentifier)
}

fn parse_stream(segments: &[&str]) -> Result<StreamId, ResourcePathError> {
    if segments.is_empty() {
        return Err(ResourcePathError::Unrecognized);
    }
    let mut stream = String::new();
    for segment in segments {
        if !stream.is_empty() {
            stream.push('/');
        }
        stream.push_str(
            percent_decode(segment)
                .ok_or(ResourcePathError::Unrecognized)?
                .as_ref(),
        );
    }
    Ok(StreamId::new(stream))
}

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

/// Percent-encodes each component while preserving a hierarchical stream ID.
pub(crate) struct PercentEncoded<'a>(pub &'a str);

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

    #[test]
    fn shared_media_is_parsed_from_the_right() {
        assert_eq!(
            ViewerPath::parse("/live/camera/3/segment/7.m4s"),
            Ok(ViewerPath {
                stream: StreamId::new("live/camera"),
                resource: ViewerResource::Media(MediaResource::Segment(
                    RenditionId(3),
                    SegmentId(7),
                    MediaSegmentFormat::Cmaf,
                )),
            })
        );
    }

    #[test]
    fn manifest_stream_names_can_overlap_media_directories() -> Result<(), ResourcePathError> {
        for name in ["init", "part", "segment"] {
            let parsed = ViewerPath::parse(&format!("/live/{name}/index.m3u8"))?;
            assert_eq!(parsed.stream.as_str(), format!("live/{name}"));
            assert_eq!(
                parsed.resource,
                ViewerResource::Hls(hls::Resource::Multivariant)
            );
        }
        Ok(())
    }

    #[test]
    fn packaging_names_and_types_stay_consistent() {
        let mut name = String::new();
        let segment =
            MediaResource::Segment(RenditionId(3), SegmentId(7), MediaSegmentFormat::WebVtt);
        assert_eq!(append_media_leaf(&mut name, segment), Some("segment/7.vtt"));
        // WebVTT ignores what the track carries, so every kind must agree.
        for kind in [MediaKind::Audio, MediaKind::Subtitle, MediaKind::Video] {
            assert_eq!(segment.content_type(kind), Some(ContentType::WebVtt));
        }
        assert!(segment.compressible());

        let initialization = MediaResource::Initialization(
            RenditionId(3),
            InitializationId(7),
            MediaSegmentFormat::MpegTs,
        );
        assert_eq!(append_media_leaf(&mut name, initialization), None);
        assert_eq!(initialization.content_type(MediaKind::Video), None);
    }

    /// Every ISO-BMFF object names the media it carries.
    ///
    /// The audio segment sharing its type with the audio initialization is
    /// Apple's rule rather than an oversight; see [\`ContentType::AudioMp4\`].
    #[test]
    fn iso_bmff_media_types_name_what_the_track_carries() {
        const CMAF: MediaSegmentFormat = MediaSegmentFormat::Cmaf;
        let init = MediaResource::Initialization(RenditionId(0), InitializationId(1), CMAF);
        let segment = MediaResource::Segment(RenditionId(0), SegmentId(1), CMAF);
        let part = MediaResource::Part(RenditionId(0), PartId(1), CMAF);

        assert_eq!(
            init.content_type(MediaKind::Video).map(ContentType::name),
            Some("video/mp4")
        );
        assert_eq!(
            init.content_type(MediaKind::Audio).map(ContentType::name),
            Some("audio/mp4")
        );
        for object in [segment, part] {
            assert_eq!(
                object.content_type(MediaKind::Video).map(ContentType::name),
                Some("video/iso.segment")
            );
            assert_eq!(
                object.content_type(MediaKind::Audio).map(ContentType::name),
                Some("audio/mp4")
            );
        }
    }

    /// ISO-BMFF has no spelling for a subtitle track, and says so.
    ///
    /// Nothing produces this combination: subtitle renditions are packaged as
    /// WebVTT. It is checked anyway because the alternative implementations —
    /// answering \`video/...\` or panicking — are both wrong in ways that only
    /// show up once somebody adds \`wvtt\` output, and by then the first is a
    /// player bug and the second is a request that kills the response.
    #[test]
    fn a_subtitle_track_has_no_iso_bmff_media_type() {
        for object in [
            MediaResource::Initialization(
                RenditionId(0),
                InitializationId(1),
                MediaSegmentFormat::Cmaf,
            ),
            MediaResource::Segment(RenditionId(0), SegmentId(1), MediaSegmentFormat::Cmaf),
            MediaResource::Part(RenditionId(0), PartId(1), MediaSegmentFormat::Cmaf),
        ] {
            assert_eq!(object.content_type(MediaKind::Subtitle), None);
        }
    }

    /// The subtitle path this origin actually serves keeps working.
    ///
    /// Not hypothetical: every WebVTT segment fetch reaches
    /// [\`MediaResource::content_type\`] with [\`MediaKind::Subtitle\`], so this is
    /// the arm under load, not a corner.
    #[test]
    fn a_webvtt_subtitle_object_is_text_vtt() {
        for object in [
            MediaResource::Initialization(
                RenditionId(2),
                InitializationId(1),
                MediaSegmentFormat::WebVtt,
            ),
            MediaResource::Segment(RenditionId(2), SegmentId(1), MediaSegmentFormat::WebVtt),
            MediaResource::Part(RenditionId(2), PartId(1), MediaSegmentFormat::WebVtt),
        ] {
            assert_eq!(
                object
                    .content_type(MediaKind::Subtitle)
                    .map(ContentType::name),
                Some("text/vtt"),
                "a subtitle rendition must keep serving text/vtt"
            );
        }
    }

    #[test]
    fn only_the_stream_prefix_is_decoded_once() -> Result<(), ResourcePathError> {
        for leaf in ["index.m3u8", "0/segment/1.m4s"] {
            let encoded = format!("/live//a%252Fb/%E2%98%83/{leaf}/");
            assert_eq!(ViewerPath::parse(&encoded)?.stream.as_str(), "live/a%2Fb/☃");
        }
        for path in [
            "/live/camera/%30/video.m3u8",
            "/live/camera/0/%73egment/1.m4s",
            "/live/camera/0/segment/%31.m4s",
            "/live/camera/index%2em3u8",
        ] {
            assert!(ViewerPath::parse(path).is_err(), "{path}");
        }
        Ok(())
    }

    #[test]
    fn viewer_paths_keep_their_size_and_component_limits() {
        let components = format!("/{}index.m3u8", "a/".repeat(MAXIMUM_PATH_SEGMENTS - 1));
        let bytes = format!(
            "/{}/index.m3u8",
            "a".repeat(MAXIMUM_PATH_BYTES - "/".len() - "/index.m3u8".len())
        );
        for valid in [&components, &bytes] {
            assert!(ViewerPath::parse(valid).is_ok());
        }
        for invalid in [
            format!("/a{components}"),
            format!("/{bytes}"),
            "/live/./camera/index.m3u8".into(),
            "/live/../camera/0/segment/1.m4s".into(),
        ] {
            assert_eq!(
                ViewerPath::parse(&invalid),
                Err(ResourcePathError::Unrecognized)
            );
        }
    }

    #[test]
    fn malformed_shared_paths_are_rejected() {
        assert_eq!(
            ViewerPath::parse("/live/camera/0/segment/nope.m4s"),
            Err(ResourcePathError::InvalidIdentifier)
        );
        assert_eq!(
            ViewerPath::parse("/live/camera/0/init/1.ts"),
            Err(ResourcePathError::Unrecognized)
        );
        assert_eq!(
            ViewerPath::parse("/live/%zz/0/segment/1.m4s"),
            Err(ResourcePathError::Unrecognized)
        );
    }

    #[test]
    fn hostile_paths_never_panic() {
        let mut state = 0x8d5e_3d8b_2c1a_4f6f_u64;
        for _ in 0..4_000 {
            let path = crate::test_fuzz::string(&mut state, 64);
            let _ = ViewerPath::parse(&path);
        }
    }
}
