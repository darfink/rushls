use std::{
    fmt::Write,
    num::{NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64},
};

use derive_more::Display;

use crate::domain::FrameRate;

use super::{AttributeList, ManifestWriteResult, validate_uri};

pub struct MultivariantPlaylistWriter<'a> {
    out: &'a mut String,
}

impl<'a> MultivariantPlaylistWriter<'a> {
    pub fn new(out: &'a mut String) -> ManifestWriteResult<Self> {
        writeln!(out, "#EXTM3U")?;
        Ok(Self { out })
    }

    pub fn version(&mut self, version: NonZeroU8) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-VERSION:{version}")?;
        Ok(self)
    }

    pub fn independent_segments(&mut self) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-INDEPENDENT-SEGMENTS")?;
        Ok(self)
    }

    pub fn rendition(&mut self, rendition: Rendition<'_>) -> ManifestWriteResult<&mut Self> {
        let mut attributes = AttributeList::new(self.out, "EXT-X-MEDIA");
        attributes.plain("TYPE", rendition.media_type)?;
        attributes.quoted("GROUP-ID", rendition.group_id)?;
        attributes.quoted("NAME", rendition.name)?;
        // Both default to NO, but a player reads them as a selection policy, so
        // this writer states them rather than leaving them to be inferred.
        attributes.boolean("DEFAULT", rendition.default)?;
        attributes.boolean("AUTOSELECT", rendition.autoselect)?;
        attributes.optional_quoted("LANGUAGE", rendition.language)?;
        // Required for CLOSED-CAPTIONS and meaningless for the types this
        // writer otherwise emits, so it is carried as a plain option rather
        // than being derived from the media type.
        if let Some(instream_id) = rendition.instream_id {
            attributes.plain("INSTREAM-ID", format_args!(r#""{instream_id}""#))?;
        }
        attributes.optional("SAMPLE-RATE", rendition.sample_rate)?;
        if let Some(channels) = rendition.channels {
            // Quoted, but a number cannot carry a character the quoting rejects.
            attributes.plain("CHANNELS", format_args!(r#""{channels}""#))?;
        }
        if let Some(uri) = rendition.uri {
            attributes.uri("URI", uri)?;
        }
        attributes.end()?;
        Ok(self)
    }

    pub fn variant(&mut self, variant: Variant<'_>) -> ManifestWriteResult<&mut Self> {
        let mut attributes = AttributeList::new(self.out, "EXT-X-STREAM-INF");
        attributes.plain("BANDWIDTH", variant.bandwidth)?;
        attributes.optional("AVERAGE-BANDWIDTH", variant.average_bandwidth)?;
        attributes.optional_quoted("CODECS", variant.codecs)?;
        if let Some((width, height)) = variant.resolution {
            attributes.plain("RESOLUTION", format_args!("{width}x{height}"))?;
        }
        if let Some(frame_rate) = variant.frame_rate {
            let rate =
                f64::from(frame_rate.numerator().get()) / f64::from(frame_rate.denominator().get());
            attributes.plain("FRAME-RATE", format_args!("{rate:.3}"))?;
        }
        attributes.optional("VIDEO-RANGE", variant.video_range)?;
        attributes.optional_quoted("VIDEO", variant.video_group_id)?;
        attributes.optional_quoted("AUDIO", variant.audio_group_id)?;
        attributes.optional_quoted("SUBTITLES", variant.subtitle_group_id)?;
        // NONE is an enumerated string rather than a quoted one, and
        // draft-pantos-hls-rfc8216bis-22 section 4.4.6.2 requires that every
        // variant agree: captions present on one variant but not another is
        // what triggers the playback inconsistencies the attribute exists to
        // prevent. The projection decides the value once for that reason.
        match variant.closed_captions {
            Some(ClosedCaptions::Group(group_id)) => {
                attributes.quoted("CLOSED-CAPTIONS", group_id)?;
            }
            Some(ClosedCaptions::None) => {
                attributes.plain("CLOSED-CAPTIONS", "NONE")?;
            }
            None => {}
        }
        attributes.end()?;

        // The URI is a line of its own rather than an attribute, so it is
        // checked by the same rule the writer applies to the ones that are.
        validate_uri(variant.uri, "EXT-X-STREAM-INF", "URI")?;
        writeln!(self.out, "{}", variant.uri)?;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum PlaylistMediaType {
    Audio,
    Video,
    Subtitles,
    ClosedCaptions,
}

/// What a variant advertises for `CLOSED-CAPTIONS`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosedCaptions<'a> {
    /// Names an `EXT-X-MEDIA` group carrying in-band caption services.
    Group(&'a str),
    /// States that no variant in this presentation carries captions.
    None,
}

/// A caption channel carried inside the video segments.
///
/// Restricted to the values section 4.4.6.1 permits: the CEA-608 Line 21 data
/// service channels, and the CEA-708 DTVCC service block numbers.
///
/// The service number is deliberately not a public field: it is only
/// constructible through [`InstreamId::service`], which is what makes the range
/// restriction an invariant of the type rather than a rule every caller has to
/// remember.
#[derive(Clone, Copy, Debug, Display, Eq, Ord, PartialEq, PartialOrd)]
pub enum InstreamId {
    #[display("CC1")]
    Cc1,
    #[display("CC2")]
    Cc2,
    #[display("CC3")]
    Cc3,
    #[display("CC4")]
    Cc4,
    #[display("SERVICE{}", _0.0)]
    Service(ServiceNumber),
}

/// A CEA-708 DTVCC service block number known to be within 1..=63.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ServiceNumber(u8);

impl ServiceNumber {
    pub fn get(self) -> u8 {
        self.0
    }
}

impl InstreamId {
    /// The highest CEA-708 service block number section 4.4.6.1 permits.
    pub const MAXIMUM_SERVICE: u8 = 63;

    /// A DTVCC service channel, or `None` outside the permitted 1..=63.
    pub fn service(number: u8) -> Option<Self> {
        (1..=Self::MAXIMUM_SERVICE)
            .contains(&number)
            .then_some(Self::Service(ServiceNumber(number)))
    }

    /// The CEA-608 channel carried by a Line 21 field, counting from zero.
    ///
    /// Field 1 carries CC1 and CC2; field 2 carries CC3 and CC4. Only the first
    /// channel of each field is reported, because distinguishing the second
    /// requires decoding the control codes rather than observing that the field
    /// carries data at all.
    pub fn for_field(field: u8) -> Option<Self> {
        match field {
            0 => Some(Self::Cc1),
            1 => Some(Self::Cc3),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum VideoRange {
    Sdr,
    Hlg,
    Pq,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rendition<'a> {
    pub media_type: PlaylistMediaType,
    pub group_id: &'a str,
    pub name: &'a str,
    pub language: Option<&'a str>,
    /// Required when the type is `CLOSED-CAPTIONS`, absent otherwise.
    pub instream_id: Option<InstreamId>,
    pub sample_rate: Option<NonZeroU32>,
    pub channels: Option<NonZeroU16>,
    pub default: bool,
    pub autoselect: bool,
    pub uri: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Variant<'a> {
    pub bandwidth: NonZeroU64,
    pub average_bandwidth: Option<NonZeroU64>,
    pub codecs: Option<&'a str>,
    pub resolution: Option<(NonZeroU32, NonZeroU32)>,
    pub frame_rate: Option<FrameRate>,
    pub video_range: Option<VideoRange>,
    pub video_group_id: Option<&'a str>,
    pub audio_group_id: Option<&'a str>,
    pub subtitle_group_id: Option<&'a str>,
    pub closed_captions: Option<ClosedCaptions<'a>>,
    pub uri: &'a str,
}

#[cfg(test)]
mod tests {
    use crate::delivery::hls::manifest::ManifestWriteError;

    use super::*;

    #[test]
    fn renders_renditions_and_variants() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MultivariantPlaylistWriter::new(&mut rendered)?;
        writer
            .version(nz::u8!(10))
            .and_then(MultivariantPlaylistWriter::independent_segments)
            .and_then(|writer| {
                writer.rendition(Rendition {
                    media_type: PlaylistMediaType::Audio,
                    group_id: "audio",
                    name: "English",
                    language: Some("en"),
                    instream_id: None,
                    sample_rate: Some(nz::u32!(48_000)),
                    channels: Some(nz::u16!(2)),
                    default: true,
                    autoselect: true,
                    uri: Some("audio/en.m3u8"),
                })
            })
            .and_then(|writer| {
                writer.variant(Variant {
                    bandwidth: nz::u64!(3_000_000),
                    average_bandwidth: Some(nz::u64!(2_500_000)),
                    codecs: Some("avc1.640028,mp4a.40.2"),
                    resolution: Some((nz::u32!(1920), nz::u32!(1080))),
                    frame_rate: Some(FrameRate::new(nz::u32!(30_000), nz::u32!(1_001))),
                    video_range: Some(VideoRange::Sdr),
                    video_group_id: None,
                    audio_group_id: Some("audio"),
                    subtitle_group_id: None,
                    closed_captions: None,
                    uri: "video/1080p.m3u8",
                })
            })?;

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-VERSION:10\n",
                "#EXT-X-INDEPENDENT-SEGMENTS\n",
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",",
                "DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"en\",SAMPLE-RATE=48000,",
                "CHANNELS=\"2\",URI=\"audio/en.m3u8\"\n",
                "#EXT-X-STREAM-INF:BANDWIDTH=3000000,AVERAGE-BANDWIDTH=2500000,",
                "CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,",
                "FRAME-RATE=29.970,VIDEO-RANGE=SDR,AUDIO=\"audio\"\n",
                "video/1080p.m3u8\n",
            )
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_values_before_starting_a_tag() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MultivariantPlaylistWriter::new(&mut rendered)?;

        assert!(
            writer
                .rendition(Rendition {
                    media_type: PlaylistMediaType::Audio,
                    group_id: "audio",
                    name: "English\"\n#EXT-X-ENDLIST",
                    language: None,
                    instream_id: None,
                    sample_rate: None,
                    channels: None,
                    default: false,
                    autoselect: false,
                    uri: None,
                })
                .is_err()
        );
        assert_eq!(rendered, "#EXTM3U\n");
        Ok(())
    }

    #[test]
    fn renders_a_closed_caption_rendition_without_a_uri() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MultivariantPlaylistWriter::new(&mut rendered)?;
        writer
            .rendition(Rendition {
                media_type: PlaylistMediaType::ClosedCaptions,
                group_id: "cc",
                name: "English",
                language: Some("en"),
                instream_id: InstreamId::service(1),
                sample_rate: None,
                channels: None,
                default: true,
                autoselect: true,
                // Section 4.4.6.1 forbids a URI on a closed-caption rendition:
                // the media is in the video segments, not a playlist of its own.
                uri: None,
            })
            .and_then(|writer| {
                writer.variant(Variant {
                    bandwidth: nz::u64!(3_000_000),
                    average_bandwidth: None,
                    codecs: Some("avc1.640028"),
                    resolution: None,
                    frame_rate: None,
                    video_range: None,
                    video_group_id: None,
                    audio_group_id: None,
                    subtitle_group_id: None,
                    closed_captions: Some(ClosedCaptions::Group("cc")),
                    uri: "video/1080p.m3u8",
                })
            })?;

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"English\",",
                "DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"en\",INSTREAM-ID=\"SERVICE1\"\n",
                "#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640028\",",
                "CLOSED-CAPTIONS=\"cc\"\n",
                "video/1080p.m3u8\n",
            )
        );
        Ok(())
    }

    #[test]
    fn renders_an_unquoted_none_for_a_presentation_without_captions()
    -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MultivariantPlaylistWriter::new(&mut rendered)?;
        writer.variant(Variant {
            bandwidth: nz::u64!(3_000_000),
            average_bandwidth: None,
            codecs: None,
            resolution: None,
            frame_rate: None,
            video_range: None,
            video_group_id: None,
            audio_group_id: None,
            subtitle_group_id: None,
            closed_captions: Some(ClosedCaptions::None),
            uri: "video/1080p.m3u8",
        })?;

        // NONE is an enumerated string; quoting it would name a group called
        // "NONE" rather than asserting the absence of captions.
        assert!(rendered.contains("CLOSED-CAPTIONS=NONE\n"));
        Ok(())
    }

    #[test]
    fn a_service_number_outside_the_permitted_range_is_refused() {
        assert_eq!(
            InstreamId::service(1).map(|id| id.to_string()).as_deref(),
            Some("SERVICE1")
        );
        assert_eq!(
            InstreamId::service(63).map(|id| id.to_string()).as_deref(),
            Some("SERVICE63")
        );
        assert_eq!(InstreamId::service(0), None);
        assert_eq!(InstreamId::service(64), None);
        // Field 1 carries CC1, field 2 carries CC3; anything else names nothing.
        assert_eq!(InstreamId::for_field(0), Some(InstreamId::Cc1));
        assert_eq!(InstreamId::for_field(1), Some(InstreamId::Cc3));
        assert_eq!(InstreamId::for_field(2), None);
    }
}
