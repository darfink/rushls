//! Reading one demuxed subtitle cue into the WebVTT form the segmenter packages.
//!
//! Every rule that differs between the cue formats this node accepts lives
//! behind [`CueDialect`], so the segmenter above never matches on [`Codec`]:
//! window cutting, sealing and rendering are identical whatever the cues
//! arrived as. Accepting a further format is a variant here plus its converter,
//! rather than edits spread across the segmentation path.

use std::{str, sync::Arc};

use crate::{
    domain::{Codec, WebVttCueMetadata},
    media::SubtitleSample,
};

use super::{MuxError, mux_error};

mod subrip;

/// One cue's WebVTT content, whatever dialect produced it.
pub(super) struct CueContent {
    pub metadata: WebVttCueMetadata,
    pub text: Arc<str>,
}

/// Which cue format one WebVTT rendition is reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CueDialect {
    /// Cues already are WebVTT; only their text and metadata are checked.
    PassThrough,
    /// SubRip markup is converted cue by cue. FFmpeg has already removed the
    /// index and timing lines, leaving inline markup over packet timing.
    SubRip,
}

impl CueDialect {
    /// The dialect for `codec`, or `None` when it cannot be presented as WebVTT.
    pub(super) fn for_codec(codec: Codec) -> Option<Self> {
        match codec {
            Codec::WebVtt => Some(Self::PassThrough),
            Codec::SubRip => Some(Self::SubRip),
            _ => None,
        }
    }

    /// Converts one cue, rejecting anything WebVTT cannot carry faithfully.
    ///
    /// Purely a function of the cue: nothing here observes segmentation state,
    /// so a rejection leaves the caller's queue untouched.
    pub(super) fn read(self, sample: &SubtitleSample) -> Result<CueContent, MuxError> {
        // Rejected for every dialect rather than dropped: WebVTT has no canvas
        // to project a pixel rectangle onto, and discarding the placement would
        // silently change how the cue presents.
        if sample.position.is_some() {
            return Err(mux_error(format!(
                "{} carries pixel-positioned subtitle text that WebVTT cannot map without a canvas",
                sample.track_id
            )));
        }
        let payload = sample.payload.as_bytes();
        match self {
            Self::PassThrough => Ok(CueContent {
                metadata: validate_metadata(sample.webvtt.clone())?,
                text: webvtt_text(payload)?,
            }),
            Self::SubRip => {
                if sample.webvtt != WebVttCueMetadata::default() {
                    return Err(mux_error(format!(
                        "{} SubRip cue carries WebVTT-only metadata",
                        sample.track_id
                    )));
                }
                Ok(CueContent {
                    metadata: WebVttCueMetadata::default(),
                    text: subrip::convert(payload)?,
                })
            }
        }
    }
}

fn validate_metadata(metadata: WebVttCueMetadata) -> Result<WebVttCueMetadata, MuxError> {
    if metadata
        .identifier
        .as_deref()
        .is_some_and(|value| value.contains(['\0', '\r', '\n']) || value.contains("-->"))
    {
        return Err(mux_error("WebVTT cue identifier is not a single safe line"));
    }
    if let Some(settings) = metadata.settings.as_deref() {
        if settings.contains(['\0', '\r', '\n']) || settings.contains("-->") {
            return Err(mux_error("WebVTT cue settings are not a single safe line"));
        }
        if settings
            .split_ascii_whitespace()
            .any(|setting| setting.starts_with("region:"))
        {
            return Err(mux_error(
                "WebVTT cue regions require a global REGION definition",
            ));
        }
    }
    Ok(metadata)
}

fn webvtt_text(bytes: &[u8]) -> Result<Arc<str>, MuxError> {
    let text = str::from_utf8(bytes).map_err(|_| mux_error("WebVTT cue text is not UTF-8"))?;
    if text.contains('\0') {
        return Err(mux_error("WebVTT cue text contains a NUL byte"));
    }
    let normalized = normalize_newlines(text);
    if normalized.is_empty() || normalized.contains("\n\n") {
        return Err(mux_error(
            "WebVTT cue text is empty or contains a blank line",
        ));
    }
    Ok(Arc::from(normalized))
}

/// Collapses the line endings a cue may arrive with onto WebVTT's single form.
///
/// Shared with [`subrip`] because a blank line terminates a cue in both
/// dialects: whichever ending the input used, the cue body has to be measured
/// against `\n` before it can be checked for one.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use crate::domain::{Payload, SubtitlePosition, TrackId};

    use super::*;

    fn sample(codec: Codec, text: &[u8]) -> SubtitleSample {
        SubtitleSample {
            track_id: TrackId(0),
            codec,
            pts: 0,
            duration: 90_000,
            webvtt: WebVttCueMetadata::default(),
            position: None,
            payload: Payload::from(text.to_vec()),
        }
    }

    #[test]
    fn only_cue_formats_with_a_converter_have_a_dialect() {
        assert_eq!(
            CueDialect::for_codec(Codec::WebVtt),
            Some(CueDialect::PassThrough)
        );
        assert_eq!(
            CueDialect::for_codec(Codec::SubRip),
            Some(CueDialect::SubRip)
        );
        assert_eq!(CueDialect::for_codec(Codec::MovText), None);
    }

    #[test]
    fn metadata_rejects_unsafe_lines_and_undefined_regions() {
        for metadata in [
            WebVttCueMetadata {
                identifier: Some(Arc::from("bad\0identifier")),
                settings: None,
            },
            WebVttCueMetadata {
                identifier: None,
                settings: Some(Arc::from("align:start\nposition:20%")),
            },
            WebVttCueMetadata {
                identifier: None,
                settings: Some(Arc::from("region:captions")),
            },
        ] {
            assert!(validate_metadata(metadata).is_err());
        }
    }

    #[test]
    fn subrip_markup_is_converted_and_its_webvtt_metadata_is_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = CueDialect::SubRip.read(&sample(Codec::SubRip, b"<B>bold</B>"))?;
        assert_eq!(content.text.as_ref(), "<b>bold</b>");
        assert_eq!(content.metadata, WebVttCueMetadata::default());

        let mut carries_metadata = sample(Codec::SubRip, b"text");
        carries_metadata.webvtt = WebVttCueMetadata {
            identifier: Some(Arc::from("cue-one")),
            settings: None,
        };
        assert!(CueDialect::SubRip.read(&carries_metadata).is_err());
        Ok(())
    }

    #[test]
    fn no_dialect_accepts_a_pixel_positioned_cue() {
        for dialect in [CueDialect::PassThrough, CueDialect::SubRip] {
            let mut positioned = sample(Codec::SubRip, b"placed");
            positioned.position = Some(SubtitlePosition {
                x1: 1,
                y1: 2,
                x2: 3,
                y2: 4,
            });
            assert!(dialect.read(&positioned).is_err());
        }
    }
}
