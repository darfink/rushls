use std::{
    fmt::Write,
    num::{NonZeroU8, NonZeroU64},
    time::{Duration, SystemTime},
};

use derive_more::Display;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{AttributeList, DecimalSeconds, ManifestWriteError, ManifestWriteResult, validate_uri};

pub struct MediaPlaylistWriter<'a, W: Write + ?Sized> {
    out: &'a mut W,
}

impl<'a, W: Write + ?Sized> MediaPlaylistWriter<'a, W> {
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

    pub fn target_duration(&mut self, seconds: NonZeroU64) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-TARGETDURATION:{seconds}")?;
        Ok(self)
    }

    pub fn media_sequence(&mut self, sequence: u64) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-MEDIA-SEQUENCE:{sequence}")?;
        Ok(self)
    }

    pub fn discontinuity_sequence(&mut self, sequence: u64) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-DISCONTINUITY-SEQUENCE:{sequence}")?;
        Ok(self)
    }

    pub fn playlist_type(
        &mut self,
        presentation: PlaylistPresentationType,
    ) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-PLAYLIST-TYPE:{presentation}")?;
        Ok(self)
    }

    pub fn part_information(&mut self, target: Duration) -> ManifestWriteResult<&mut Self> {
        require_nonzero(target, "PART-TARGET")?;
        writeln!(
            self.out,
            "#EXT-X-PART-INF:PART-TARGET={}",
            DecimalSeconds(target)
        )?;
        Ok(self)
    }

    pub fn server_control(&mut self, control: ServerControl) -> ManifestWriteResult<&mut Self> {
        if control.is_empty() {
            return Err(ManifestWriteError::EmptyServerControl);
        }
        if let Some(duration) = control.hold_back {
            require_nonzero(duration, "HOLD-BACK")?;
        }
        if let Some(duration) = control.part_hold_back {
            require_nonzero(duration, "PART-HOLD-BACK")?;
        }
        if let Some(duration) = control.can_skip_until {
            require_nonzero(duration, "CAN-SKIP-UNTIL")?;
        }

        self.out.write_str("#EXT-X-SERVER-CONTROL:")?;
        let mut attributes = AttributeList::new(self.out);
        if let Some(duration) = control.hold_back {
            attributes.item(|out| write!(out, "HOLD-BACK={}", DecimalSeconds(duration)))?;
        }
        if let Some(duration) = control.part_hold_back {
            attributes.item(|out| write!(out, "PART-HOLD-BACK={}", DecimalSeconds(duration)))?;
        }
        if control.can_block_reload {
            attributes.item(|out| out.write_str("CAN-BLOCK-RELOAD=YES"))?;
        }
        if let Some(duration) = control.can_skip_until {
            attributes.item(|out| write!(out, "CAN-SKIP-UNTIL={}", DecimalSeconds(duration)))?;
        }
        if control.can_skip_dateranges {
            attributes.item(|out| out.write_str("CAN-SKIP-DATERANGES=YES"))?;
        }
        self.out.write_char('\n')?;
        Ok(self)
    }

    pub fn initialization_map(&mut self, uri: &str) -> ManifestWriteResult<&mut Self> {
        validate_uri(uri, "EXT-X-MAP URI")?;
        writeln!(self.out, r#"#EXT-X-MAP:URI="{uri}""#)?;
        Ok(self)
    }

    pub fn discontinuity(&mut self) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-DISCONTINUITY")?;
        Ok(self)
    }

    pub fn program_date_time(&mut self, time: SystemTime) -> ManifestWriteResult<&mut Self> {
        let formatted = OffsetDateTime::from(time).format(&Rfc3339)?;
        writeln!(self.out, "#EXT-X-PROGRAM-DATE-TIME:{formatted}")?;
        Ok(self)
    }

    pub fn part(&mut self, part: Part<'_>) -> ManifestWriteResult<&mut Self> {
        validate_uri(part.uri, "EXT-X-PART URI")?;
        require_nonzero(part.duration, "EXT-X-PART DURATION")?;

        self.out.write_str("#EXT-X-PART:")?;
        let mut attributes = AttributeList::new(self.out);
        attributes.item(|out| write!(out, "DURATION={}", DecimalSeconds(part.duration)))?;
        attributes.item(|out| write!(out, r#"URI="{}""#, part.uri))?;
        if part.independent {
            attributes.item(|out| out.write_str("INDEPENDENT=YES"))?;
        }
        if part.gap {
            attributes.item(|out| out.write_str("GAP=YES"))?;
        }
        self.out.write_char('\n')?;
        Ok(self)
    }

    pub fn segment(&mut self, segment: Segment<'_>) -> ManifestWriteResult<&mut Self> {
        validate_uri(segment.uri, "segment URI")?;
        require_nonzero(segment.duration, "EXTINF")?;
        if segment.gap {
            writeln!(self.out, "#EXT-X-GAP")?;
        }
        writeln!(self.out, "#EXTINF:{},", DecimalSeconds(segment.duration))?;
        writeln!(self.out, "{}", segment.uri)?;
        Ok(self)
    }

    pub fn skipped_segments(&mut self, count: NonZeroU64) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-SKIP:SKIPPED-SEGMENTS={count}")?;
        Ok(self)
    }

    pub fn rendition_report(
        &mut self,
        report: RenditionReport<'_>,
    ) -> ManifestWriteResult<&mut Self> {
        validate_uri(report.uri, "EXT-X-RENDITION-REPORT URI")?;
        self.out.write_str("#EXT-X-RENDITION-REPORT:")?;
        let mut attributes = AttributeList::new(self.out);
        attributes.item(|out| write!(out, r#"URI="{}""#, report.uri))?;
        if let Some(sequence) = report.last_media_sequence {
            attributes.item(|out| write!(out, "LAST-MSN={sequence}"))?;
        }
        if let Some(part) = report.last_part {
            attributes.item(|out| write!(out, "LAST-PART={part}"))?;
        }
        self.out.write_char('\n')?;
        Ok(self)
    }

    pub fn preload_hint(&mut self, hint: PreloadHint<'_>) -> ManifestWriteResult<&mut Self> {
        validate_uri(hint.uri, "EXT-X-PRELOAD-HINT URI")?;
        writeln!(
            self.out,
            r#"#EXT-X-PRELOAD-HINT:TYPE={},URI="{}""#,
            hint.hint_type, hint.uri
        )?;
        Ok(self)
    }

    pub fn endlist(&mut self) -> ManifestWriteResult<&mut Self> {
        writeln!(self.out, "#EXT-X-ENDLIST")?;
        Ok(self)
    }
}

fn require_nonzero(duration: Duration, field: &'static str) -> ManifestWriteResult<()> {
    if duration.is_zero() {
        return Err(ManifestWriteError::ZeroDuration { field });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum PlaylistPresentationType {
    Event,
    Vod,
}

#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum PreloadHintType {
    Part,
    Map,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServerControl {
    pub hold_back: Option<Duration>,
    pub part_hold_back: Option<Duration>,
    pub can_block_reload: bool,
    pub can_skip_until: Option<Duration>,
    pub can_skip_dateranges: bool,
}

impl ServerControl {
    fn is_empty(self) -> bool {
        self.hold_back.is_none()
            && self.part_hold_back.is_none()
            && !self.can_block_reload
            && self.can_skip_until.is_none()
            && !self.can_skip_dateranges
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Part<'a> {
    pub uri: &'a str,
    pub duration: Duration,
    pub independent: bool,
    pub gap: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment<'a> {
    pub uri: &'a str,
    pub duration: Duration,
    pub gap: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenditionReport<'a> {
    pub uri: &'a str,
    pub last_media_sequence: Option<u64>,
    pub last_part: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreloadHint<'a> {
    pub hint_type: PreloadHintType,
    pub uri: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_low_latency_media_playlist() {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered).expect("header renders");
        writer
            .version(NonZeroU8::new(10).expect("constant"))
            .and_then(|writer| writer.target_duration(NonZeroU64::new(2).expect("constant")))
            .and_then(|writer| writer.media_sequence(41))
            .and_then(|writer| writer.discontinuity_sequence(3))
            .and_then(|writer| writer.part_information(Duration::from_millis(200)))
            .and_then(|writer| {
                writer.server_control(ServerControl {
                    part_hold_back: Some(Duration::from_millis(600)),
                    can_block_reload: true,
                    ..ServerControl::default()
                })
            })
            .and_then(|writer| writer.initialization_map("init/7.mp4"))
            .and_then(|writer| {
                writer.part(Part {
                    uri: "part/42/0.m4s",
                    duration: Duration::from_millis(200),
                    independent: true,
                    gap: false,
                })
            })
            .and_then(|writer| {
                writer.preload_hint(PreloadHint {
                    hint_type: PreloadHintType::Part,
                    uri: "part/42/1.m4s",
                })
            })
            .expect("playlist renders");

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-VERSION:10\n",
                "#EXT-X-TARGETDURATION:2\n",
                "#EXT-X-MEDIA-SEQUENCE:41\n",
                "#EXT-X-DISCONTINUITY-SEQUENCE:3\n",
                "#EXT-X-PART-INF:PART-TARGET=0.2\n",
                "#EXT-X-SERVER-CONTROL:PART-HOLD-BACK=0.6,CAN-BLOCK-RELOAD=YES\n",
                "#EXT-X-MAP:URI=\"init/7.mp4\"\n",
                "#EXT-X-PART:DURATION=0.2,URI=\"part/42/0.m4s\",INDEPENDENT=YES\n",
                "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/42/1.m4s\"\n",
            )
        );
    }

    #[test]
    fn renders_discontinuities_gaps_and_a_terminal_segment() {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered).expect("header renders");
        writer
            .discontinuity()
            .and_then(|writer| {
                writer.segment(Segment {
                    uri: "segment/9.m4s",
                    duration: Duration::from_millis(1_999),
                    gap: true,
                })
            })
            .and_then(MediaPlaylistWriter::endlist)
            .expect("playlist renders");

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-DISCONTINUITY\n",
                "#EXT-X-GAP\n",
                "#EXTINF:1.999,\n",
                "segment/9.m4s\n",
                "#EXT-X-ENDLIST\n",
            )
        );
    }

    #[test]
    fn rejects_an_unsafe_uri_before_writing_its_tag() {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered).expect("header renders");

        assert!(matches!(
            writer.initialization_map("init.mp4\"\n#EXT-X-ENDLIST"),
            Err(ManifestWriteError::InvalidUri { .. })
        ));
        assert_eq!(rendered, "#EXTM3U\n");
    }

    #[test]
    fn renders_program_date_time_as_rfc3339_utc() {
        let timestamp = SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(1_700_000_000, 123_456_789))
            .expect("test timestamp is representable");
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered).expect("header renders");

        writer
            .program_date_time(timestamp)
            .expect("timestamp renders");

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:20.123456789Z\n",
            )
        );
    }
}
