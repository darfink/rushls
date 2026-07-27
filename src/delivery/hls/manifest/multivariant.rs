use std::{
    fmt::Write,
    num::{NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64},
};

use derive_more::Display;

use crate::domain::FrameRate;

use super::{AttributeList, ManifestWriteResult, validate_quoted, validate_uri};

pub struct MultivariantPlaylistWriter<'a, W: Write + ?Sized> {
    out: &'a mut W,
}

impl<'a, W: Write + ?Sized> MultivariantPlaylistWriter<'a, W> {
    pub fn new(out: &'a mut W) -> ManifestWriteResult<Self> {
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
        validate_quoted(rendition.group_id, "GROUP-ID")?;
        validate_quoted(rendition.name, "NAME")?;
        if let Some(language) = rendition.language {
            validate_quoted(language, "LANGUAGE")?;
        }
        if let Some(uri) = rendition.uri {
            validate_uri(uri, "EXT-X-MEDIA URI")?;
        }

        self.out.write_str("#EXT-X-MEDIA:")?;
        let mut attributes = AttributeList::new(self.out);
        attributes.item(|out| write!(out, "TYPE={}", rendition.media_type))?;
        attributes.item(|out| write!(out, r#"GROUP-ID="{}""#, rendition.group_id))?;
        attributes.item(|out| write!(out, r#"NAME="{}""#, rendition.name))?;
        attributes.item(|out| {
            write!(
                out,
                "DEFAULT={}",
                if rendition.default { "YES" } else { "NO" }
            )
        })?;
        attributes.item(|out| {
            write!(
                out,
                "AUTOSELECT={}",
                if rendition.autoselect { "YES" } else { "NO" }
            )
        })?;
        if let Some(language) = rendition.language {
            attributes.item(|out| write!(out, r#"LANGUAGE="{language}""#))?;
        }
        if let Some(sample_rate) = rendition.sample_rate {
            attributes.item(|out| write!(out, "SAMPLE-RATE={sample_rate}"))?;
        }
        if let Some(channels) = rendition.channels {
            attributes.item(|out| write!(out, r#"CHANNELS="{channels}""#))?;
        }
        if let Some(uri) = rendition.uri {
            attributes.item(|out| write!(out, r#"URI="{uri}""#))?;
        }
        self.out.write_char('\n')?;
        Ok(self)
    }

    pub fn variant(&mut self, variant: Variant<'_>) -> ManifestWriteResult<&mut Self> {
        validate_uri(variant.uri, "variant URI")?;
        for (value, field) in [
            (variant.codecs, "CODECS"),
            (variant.video_group_id, "VIDEO"),
            (variant.audio_group_id, "AUDIO"),
            (variant.subtitle_group_id, "SUBTITLES"),
        ] {
            if let Some(value) = value {
                validate_quoted(value, field)?;
            }
        }

        self.out.write_str("#EXT-X-STREAM-INF:")?;
        let mut attributes = AttributeList::new(self.out);
        attributes.item(|out| write!(out, "BANDWIDTH={}", variant.bandwidth))?;
        if let Some(bandwidth) = variant.average_bandwidth {
            attributes.item(|out| write!(out, "AVERAGE-BANDWIDTH={bandwidth}"))?;
        }
        if let Some(codecs) = variant.codecs {
            attributes.item(|out| write!(out, r#"CODECS="{codecs}""#))?;
        }
        if let Some((width, height)) = variant.resolution {
            attributes.item(|out| write!(out, "RESOLUTION={width}x{height}"))?;
        }
        if let Some(frame_rate) = variant.frame_rate {
            let value =
                f64::from(frame_rate.numerator().get()) / f64::from(frame_rate.denominator().get());
            attributes.item(|out| write!(out, "FRAME-RATE={value:.3}"))?;
        }
        if let Some(range) = variant.video_range {
            attributes.item(|out| write!(out, "VIDEO-RANGE={range}"))?;
        }
        if let Some(group) = variant.video_group_id {
            attributes.item(|out| write!(out, r#"VIDEO="{group}""#))?;
        }
        if let Some(group) = variant.audio_group_id {
            attributes.item(|out| write!(out, r#"AUDIO="{group}""#))?;
        }
        if let Some(group) = variant.subtitle_group_id {
            attributes.item(|out| write!(out, r#"SUBTITLES="{group}""#))?;
        }
        self.out.write_char('\n')?;
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
    pub uri: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_renditions_and_variants() {
        let mut rendered = String::new();
        let mut writer = MultivariantPlaylistWriter::new(&mut rendered).expect("header renders");
        writer
            .version(nz::u8!(10))
            .and_then(MultivariantPlaylistWriter::independent_segments)
            .and_then(|writer| {
                writer.rendition(Rendition {
                    media_type: PlaylistMediaType::Audio,
                    group_id: "audio",
                    name: "English",
                    language: Some("en"),
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
                    uri: "video/1080p.m3u8",
                })
            })
            .expect("playlist renders");

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
    }

    #[test]
    fn rejects_invalid_values_before_starting_a_tag() {
        let mut rendered = String::new();
        let mut writer = MultivariantPlaylistWriter::new(&mut rendered).expect("header renders");

        assert!(
            writer
                .rendition(Rendition {
                    media_type: PlaylistMediaType::Audio,
                    group_id: "audio",
                    name: "English\"\n#EXT-X-ENDLIST",
                    language: None,
                    sample_rate: None,
                    channels: None,
                    default: false,
                    autoselect: false,
                    uri: None,
                })
                .is_err()
        );
        assert_eq!(rendered, "#EXTM3U\n");
    }
}
