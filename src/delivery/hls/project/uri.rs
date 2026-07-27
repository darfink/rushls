//! The resource names delivery serves and playlists reference.
//!
//! One definition, shared by the projection that writes these names and the
//! router that resolves them. Keeping them together is the point: a playlist
//! naming a resource the router cannot parse is a class of bug that only shows
//! up as a 404 in a player's network log.
//!
//! Every name a playlist emits is **relative to the playlist that emits it**, so
//! nothing here knows a host, a scheme, or a deployment path prefix. A media
//! playlist at `.../{rendition}/media.m3u8` names its own media as
//! `segment/7.m4s` and a sibling as `../3/media.m3u8`. That makes the same bytes
//! correct behind any prefix, and it keeps the projection free of request
//! context it would otherwise have to be handed.
//!
//! Resource names are built from durable store identities rather than media
//! sequence numbers. An ID names one immutable object for as long as it is
//! fetchable, which is what makes segment and part responses cacheable forever
//! and what lets a URL outlive the playlist tag that introduced it.

use std::{
    borrow::Cow,
    fmt::{self, Display, Write},
};

use derive_more::Display;
use urlencoding::{Encoded, decode};

use crate::{
    delivery::hls::{InitializationId, PartId, SegmentId},
    domain::RenditionId,
    mux::MediaSegmentFormat,
};

/// The file name a multivariant playlist is served as.
pub const MULTIVARIANT_NAME: &str = "master.m3u8";
/// The file name every media playlist is served as.
pub const MEDIA_PLAYLIST_NAME: &str = "media.m3u8";

const INITIALIZATION_DIRECTORY: &str = "init";
const SEGMENT_DIRECTORY: &str = "segment";
const PART_DIRECTORY: &str = "part";

/// One addressable delivery resource within a stream.
///
/// Deliberately does not carry the stream: a stream identity is an arbitrary
/// operator-chosen string that needs percent-encoding and lives in the router's
/// half of the path, while everything here is a fixed word or a number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resource {
    Multivariant,
    MediaPlaylist(RenditionId),
    Initialization(RenditionId, InitializationId),
    Segment(RenditionId, SegmentId),
    Part(RenditionId, PartId),
}

impl Resource {
    /// The rendition this resource belongs to, if it is not stream-wide.
    pub fn rendition(&self) -> Option<RenditionId> {
        match self {
            Self::Multivariant => None,
            Self::MediaPlaylist(rendition)
            | Self::Initialization(rendition, _)
            | Self::Segment(rendition, _)
            | Self::Part(rendition, _) => Some(*rendition),
        }
    }
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

    pub fn segment_extension(&self) -> &'static str {
        self.segment
    }

    /// Names one resource as a media playlist would reference it.
    pub fn media_relative(&self, resource: Resource) -> Option<String> {
        let mut out = String::new();
        self.write_media_relative(&mut out, resource).ok()?;
        Some(out)
    }

    fn write_media_relative(
        &self,
        out: &mut impl Write,
        resource: Resource,
    ) -> Result<(), NotRelative> {
        match resource {
            // A media playlist references its siblings by stepping out of its
            // own rendition directory, which is what keeps these names free of
            // any absolute prefix.
            Resource::MediaPlaylist(rendition) => {
                write!(out, "../{}/{MEDIA_PLAYLIST_NAME}", rendition.0).map_err(|_| NotRelative)
            }
            Resource::Initialization(_, initialization) => {
                let extension = self.initialization.ok_or(NotRelative)?;
                write!(
                    out,
                    "{INITIALIZATION_DIRECTORY}/{}.{extension}",
                    initialization.0
                )
                .map_err(|_| NotRelative)
            }
            Resource::Segment(_, segment) => {
                write!(out, "{SEGMENT_DIRECTORY}/{}.{}", segment.0, self.segment)
                    .map_err(|_| NotRelative)
            }
            Resource::Part(_, part) => {
                write!(out, "{PART_DIRECTORY}/{}.{}", part.0, self.segment).map_err(|_| NotRelative)
            }
            // A multivariant playlist sits above every rendition directory, so
            // it is not something a media playlist has occasion to name.
            Resource::Multivariant => Err(NotRelative),
        }
    }
}

/// The resource cannot be named from where the caller is naming it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NotRelative;

/// Names one rendition's media playlist as a multivariant playlist would.
pub fn multivariant_relative(rendition: RenditionId) -> String {
    format!("{}/{MEDIA_PLAYLIST_NAME}", rendition.0)
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
    match segments {
        [.., name] if *name == MULTIVARIANT_NAME => Ok((Resource::Multivariant, 1)),
        [.., rendition, name] if *name == MEDIA_PLAYLIST_NAME => {
            Ok((Resource::MediaPlaylist(parse_rendition(rendition)?), 2))
        }
        [.., rendition, directory, file] => {
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
        _ => Err(ResourcePathError::Unrecognized),
    }
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
        Resource::Multivariant | Resource::MediaPlaylist(_) => true,
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

    #[test]
    fn media_playlists_name_their_own_media_relatively() {
        let rendition = RenditionId(3);

        assert_eq!(
            cmaf().media_relative(Resource::Segment(rendition, SegmentId(7))),
            Some("segment/7.m4s".to_owned())
        );
        assert_eq!(
            cmaf().media_relative(Resource::Part(rendition, PartId(41))),
            Some("part/41.m4s".to_owned())
        );
        assert_eq!(
            cmaf().media_relative(Resource::Initialization(rendition, InitializationId(1))),
            Some("init/1.mp4".to_owned())
        );
        assert_eq!(
            cmaf().media_relative(Resource::MediaPlaylist(RenditionId(9))),
            Some("../9/media.m3u8".to_owned()),
            "a rendition report steps out of its own directory"
        );
    }

    #[test]
    fn webvtt_names_both_its_header_and_its_cues_as_vtt() {
        let naming = ResourceNaming::for_format(MediaSegmentFormat::WebVtt);
        let rendition = RenditionId(0);

        assert_eq!(
            naming.media_relative(Resource::Initialization(rendition, InitializationId(1))),
            Some("init/1.vtt".to_owned())
        );
        assert_eq!(
            naming.media_relative(Resource::Segment(rendition, SegmentId(2))),
            Some("segment/2.vtt".to_owned())
        );
    }

    #[test]
    fn a_format_without_an_initialization_section_cannot_name_one() {
        let naming = ResourceNaming::for_format(MediaSegmentFormat::MpegTs);

        assert!(!naming.has_initialization());
        assert_eq!(
            naming.media_relative(Resource::Initialization(
                RenditionId(0),
                InitializationId(1)
            )),
            None
        );
    }

    #[test]
    fn paths_are_parsed_from_the_right_so_a_stream_may_contain_slashes() {
        assert_eq!(
            parse_tail(&["live", "camera", "master.m3u8"]),
            Ok((Resource::Multivariant, 1))
        );
        assert_eq!(
            parse_tail(&["live", "camera", "3", "media.m3u8"]),
            Ok((Resource::MediaPlaylist(RenditionId(3)), 2))
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
    fn unparseable_paths_are_refused_rather_than_guessed_at() {
        assert_eq!(
            parse_tail(&["3", "media.m3u8x"]),
            Err(ResourcePathError::Unrecognized)
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
            parse_tail(&["master.m3u8x"]),
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
