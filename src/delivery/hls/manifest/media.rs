use std::{
    fmt::Write,
    num::{NonZeroU8, NonZeroU64},
    time::{Duration, SystemTime},
};

use derive_more::Display;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{AttributeList, DecimalSeconds, ManifestWriteError, ManifestWriteResult, validate_uri};

pub struct MediaPlaylistWriter<'a> {
    out: &'a mut String,
}

impl<'a> MediaPlaylistWriter<'a> {
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
        // A hold-back of zero would tell a client to play from the live edge
        // itself, which is the one position it can never sustain.
        for (duration, field) in [
            (control.hold_back, "HOLD-BACK"),
            (control.part_hold_back, "PART-HOLD-BACK"),
            (control.can_skip_until, "CAN-SKIP-UNTIL"),
        ] {
            if let Some(duration) = duration {
                require_nonzero(duration, field)?;
            }
        }

        let mut attributes = AttributeList::new(self.out, "EXT-X-SERVER-CONTROL");
        attributes.optional("HOLD-BACK", control.hold_back.map(DecimalSeconds))?;
        attributes.optional("PART-HOLD-BACK", control.part_hold_back.map(DecimalSeconds))?;
        attributes.flag("CAN-BLOCK-RELOAD", control.can_block_reload)?;
        attributes.optional("CAN-SKIP-UNTIL", control.can_skip_until.map(DecimalSeconds))?;
        attributes.flag("CAN-SKIP-DATERANGES", control.can_skip_dateranges)?;
        // A control advertising nothing is refused by the list itself.
        attributes.end()?;
        Ok(self)
    }

    pub fn initialization_map(&mut self, uri: &str) -> ManifestWriteResult<&mut Self> {
        let mut attributes = AttributeList::new(self.out, "EXT-X-MAP");
        attributes.uri("URI", uri)?;
        attributes.end()?;
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
        require_nonzero(part.duration, "EXT-X-PART DURATION")?;

        let mut attributes = AttributeList::new(self.out, "EXT-X-PART");
        attributes.plain("DURATION", DecimalSeconds(part.duration))?;
        attributes.uri("URI", part.uri)?;
        attributes.flag("INDEPENDENT", part.independent)?;
        attributes.flag("GAP", part.gap)?;
        attributes.end()?;
        Ok(self)
    }

    pub fn segment(&mut self, segment: Segment<'_>) -> ManifestWriteResult<&mut Self> {
        validate_uri(segment.uri, "EXTINF", "URI")?;
        require_nonzero(segment.duration, "EXTINF")?;
        if segment.gap {
            writeln!(self.out, "#EXT-X-GAP")?;
        }
        writeln!(self.out, "#EXTINF:{},", DecimalSeconds(segment.duration))?;
        writeln!(self.out, "{}", segment.uri)?;
        Ok(self)
    }

    pub fn skipped_segments(
        &mut self,
        count: NonZeroU64,
        recently_removed_dateranges: Option<&str>,
    ) -> ManifestWriteResult<&mut Self> {
        let mut attributes = AttributeList::new(self.out, "EXT-X-SKIP");
        attributes.plain("SKIPPED-SEGMENTS", count)?;
        attributes.optional_quoted("RECENTLY-REMOVED-DATERANGES", recently_removed_dateranges)?;
        attributes.end()?;
        Ok(self)
    }

    pub fn rendition_report(
        &mut self,
        report: RenditionReport<'_>,
    ) -> ManifestWriteResult<&mut Self> {
        let mut attributes = AttributeList::new(self.out, "EXT-X-RENDITION-REPORT");
        attributes.uri("URI", report.uri)?;
        attributes.optional("LAST-MSN", report.last_media_sequence)?;
        attributes.optional("LAST-PART", report.last_part)?;
        attributes.end()?;
        Ok(self)
    }

    pub fn preload_hint(&mut self, hint: PreloadHint<'_>) -> ManifestWriteResult<&mut Self> {
        let mut attributes = AttributeList::new(self.out, "EXT-X-PRELOAD-HINT");
        attributes.plain("TYPE", hint.hint_type)?;
        attributes.uri("URI", hint.uri)?;
        attributes.end()?;
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
    /// Written only as `YES`. Absence already means no; `NO` is not a spec value.
    pub can_skip_dateranges: bool,
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
    fn renders_a_low_latency_media_playlist() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered)?;
        writer
            .version(nz::u8!(10))
            .and_then(|writer| writer.target_duration(nz::u64!(2)))
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
            })?;

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
        Ok(())
    }

    #[test]
    fn renders_discontinuities_gaps_and_a_terminal_segment() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered)?;
        writer
            .discontinuity()
            .and_then(|writer| {
                writer.segment(Segment {
                    uri: "segment/9.m4s",
                    duration: Duration::from_millis(1_999),
                    gap: true,
                })
            })
            .and_then(MediaPlaylistWriter::endlist)?;

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
        Ok(())
    }

    #[test]
    fn rejects_an_unsafe_uri_before_writing_its_tag() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered)?;

        assert!(matches!(
            writer.initialization_map("init.mp4\"\n#EXT-X-ENDLIST"),
            Err(ManifestWriteError::InvalidUri { .. })
        ));
        assert_eq!(rendered, "#EXTM3U\n");
        Ok(())
    }

    #[test]
    fn renders_program_date_time_as_rfc3339_utc() -> Result<(), Box<dyn std::error::Error>> {
        let timestamp = SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(1_700_000_000, 123_456_789))
            .ok_or_else(|| std::io::Error::other("test timestamp is representable"))?;
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered)?;

        writer.program_date_time(timestamp)?;

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:20.123456789Z\n",
            )
        );
        Ok(())
    }

    #[test]
    fn renders_a_skip_tag_and_an_empty_daterange_attribute() -> Result<(), ManifestWriteError> {
        let mut rendered = String::new();
        let mut writer = MediaPlaylistWriter::new(&mut rendered)?;
        writer.skipped_segments(nz::u64!(4), None)?;
        writer.skipped_segments(nz::u64!(4), Some(""))?;

        assert_eq!(
            rendered,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-SKIP:SKIPPED-SEGMENTS=4\n",
                "#EXT-X-SKIP:SKIPPED-SEGMENTS=4,RECENTLY-REMOVED-DATERANGES=\"\"\n",
            )
        );
        Ok(())
    }
}
