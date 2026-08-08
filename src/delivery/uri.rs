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
    delivery::store::{InitializationId, PartId, SegmentId},
    domain::{RenditionId, StreamId},
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentType {
    Mp4,
    IsoSegment,
    MpegTs,
    WebVtt,
    /// Text emitted by a manifest adapter, with its protocol media type.
    Manifest(&'static str),
}

impl ContentType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::IsoSegment => "video/iso.segment",
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

    pub fn format(self) -> MediaSegmentFormat {
        match self {
            Self::Initialization(_, _, format)
            | Self::Segment(_, _, format)
            | Self::Part(_, _, format) => format,
        }
    }

    pub fn content_type(self) -> Option<ContentType> {
        match self {
            Self::Initialization(_, _, format) => spellings(format)
                .initialization
                .map(|spelling| spelling.content_type),
            Self::Segment(_, _, format) | Self::Part(_, _, format) => {
                Some(spellings(format).segment.content_type)
            }
        }
    }

    pub fn compressible(self) -> bool {
        self.content_type().is_some_and(ContentType::is_text)
    }
}

/// Whether media in this packaging is text and may have a gzip representation.
pub fn is_text(format: MediaSegmentFormat) -> bool {
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
    content_type: ContentType,
}

const fn spellings(format: MediaSegmentFormat) -> Spellings {
    match format {
        MediaSegmentFormat::Cmaf => Spellings {
            initialization: Some(Spelling {
                extension: "mp4",
                content_type: ContentType::Mp4,
            }),
            segment: Spelling {
                extension: "m4s",
                content_type: ContentType::IsoSegment,
            },
        },
        // WebVTT's header is a real initialization object carried by EXT-X-MAP.
        MediaSegmentFormat::WebVtt => Spellings {
            initialization: Some(Spelling {
                extension: "vtt",
                content_type: ContentType::WebVtt,
            }),
            segment: Spelling {
                extension: "vtt",
                content_type: ContentType::WebVtt,
            },
        },
        // MPEG-TS segments are self-describing.
        MediaSegmentFormat::MpegTs => Spellings {
            initialization: None,
            segment: Spelling {
                extension: "ts",
                content_type: ContentType::MpegTs,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaResourcePath {
    pub stream: StreamId,
    pub resource: MediaResource,
}

const MAXIMUM_PATH_BYTES: usize = 2_048;
const MAXIMUM_PATH_SEGMENTS: usize = 32;

/// Resolves a shared media path, leaving manifest paths to their adapter.
pub fn parse_media_path(path: &str) -> Result<Option<MediaResourcePath>, ResourcePathError> {
    let segment_count = validate_path(path)?;
    let mut tail = path.split('/').filter(|segment| !segment.is_empty()).rev();
    let Some(name) = tail.next() else {
        return Ok(None);
    };
    let Some(directory) = tail.next() else {
        return Ok(None);
    };
    if !matches!(
        directory,
        INITIALIZATION_DIRECTORY | SEGMENT_DIRECTORY | PART_DIRECTORY
    ) {
        return Ok(None);
    }
    let rendition = tail.next().ok_or(ResourcePathError::Unrecognized)?;
    let (identifier, extension) = name
        .rsplit_once('.')
        .ok_or(ResourcePathError::Unrecognized)?;
    let format = spelled_by(directory, extension).ok_or(ResourcePathError::Unrecognized)?;
    let rendition = parse_rendition(rendition)?;
    let identifier = identifier
        .parse::<u64>()
        .map_err(|_| ResourcePathError::InvalidIdentifier)?;
    let resource = match directory {
        INITIALIZATION_DIRECTORY => {
            MediaResource::Initialization(rendition, InitializationId(identifier), format)
        }
        SEGMENT_DIRECTORY => MediaResource::Segment(rendition, SegmentId(identifier), format),
        PART_DIRECTORY => MediaResource::Part(rendition, PartId(identifier), format),
        _ => unreachable!("the directory was matched above"),
    };
    Ok(Some(MediaResourcePath {
        stream: parse_stream(path, segment_count - 3)?,
        resource,
    }))
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

fn parse_rendition(value: &str) -> Result<RenditionId, ResourcePathError> {
    value
        .parse()
        .map(RenditionId)
        .map_err(|_| ResourcePathError::InvalidIdentifier)
}

pub(crate) fn validate_path(path: &str) -> Result<usize, ResourcePathError> {
    if path.len() > MAXIMUM_PATH_BYTES {
        return Err(ResourcePathError::Unrecognized);
    }
    let mut count = 0;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        count += 1;
        if count > MAXIMUM_PATH_SEGMENTS || matches!(segment, "." | "..") {
            return Err(ResourcePathError::Unrecognized);
        }
    }
    Ok(count)
}

pub(crate) fn parse_stream(
    path: &str,
    stream_segments: usize,
) -> Result<StreamId, ResourcePathError> {
    if stream_segments == 0 {
        return Err(ResourcePathError::Unrecognized);
    }
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
            parse_media_path("/live/camera/3/segment/7.m4s"),
            Ok(Some(MediaResourcePath {
                stream: StreamId::new("live/camera"),
                resource: MediaResource::Segment(
                    RenditionId(3),
                    SegmentId(7),
                    MediaSegmentFormat::Cmaf,
                ),
            }))
        );
        assert_eq!(parse_media_path("/live/camera/index.m3u8"), Ok(None));
    }

    #[test]
    fn packaging_names_and_types_stay_consistent() {
        let mut name = String::new();
        let segment =
            MediaResource::Segment(RenditionId(3), SegmentId(7), MediaSegmentFormat::WebVtt);
        assert_eq!(append_media_leaf(&mut name, segment), Some("segment/7.vtt"));
        assert_eq!(segment.content_type(), Some(ContentType::WebVtt));
        assert!(segment.compressible());

        let initialization = MediaResource::Initialization(
            RenditionId(3),
            InitializationId(7),
            MediaSegmentFormat::MpegTs,
        );
        assert_eq!(append_media_leaf(&mut name, initialization), None);
        assert_eq!(initialization.content_type(), None);
    }

    #[test]
    fn malformed_shared_paths_are_rejected() {
        assert_eq!(
            parse_media_path("/live/camera/0/segment/nope.m4s"),
            Err(ResourcePathError::InvalidIdentifier)
        );
        assert_eq!(
            parse_media_path("/live/camera/0/init/1.ts"),
            Err(ResourcePathError::Unrecognized)
        );
        assert_eq!(
            parse_media_path("/live/%zz/0/segment/1.m4s"),
            Err(ResourcePathError::Unrecognized)
        );
    }

    #[test]
    fn hostile_paths_never_panic() {
        let mut state = 0x8d5e_3d8b_2c1a_4f6f_u64;
        for _ in 0..4_000 {
            let path = crate::test_fuzz::string(&mut state, 64);
            // Whatever the answer, the parser must answer: valid, invalid, or
            // "not mine" — never a panic.
            let _ = parse_media_path(&path);
        }
    }
}
