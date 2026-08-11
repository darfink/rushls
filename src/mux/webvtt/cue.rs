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

/// What one demuxed cue asks the segmenter to do.
///
/// Every dialect but [`CueDialect::Text`] can only ever produce
/// [`Self::Show`]: their cues carry an explicit end, so "stop displaying" is
/// expressed by that end arriving rather than by a separate signal.
pub(super) enum CueAction {
    /// Display this content until something replaces it or its end arrives.
    Show(CueContent),
    /// End whatever is on screen, and display nothing in its place.
    ///
    /// Only open-ended dialects need this. A cue with no end shows until it is
    /// replaced, so without an explicit clear the only way to stop displaying
    /// one is to send another — which is why a transport that cannot express
    /// "nothing" leaves the last caption of a pause on screen.
    Clear,
}

/// Which cue format one WebVTT rendition is reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CueDialect {
    /// Cues already are WebVTT; only their text and metadata are checked.
    PassThrough,
    /// SubRip markup is converted cue by cue. FFmpeg has already removed the
    /// index and timing lines, leaving inline markup over packet timing.
    SubRip,
    /// Bare UTF-8 with no markup, as FLV script-data captions arrive.
    ///
    /// Distinct from [`Self::PassThrough`] despite sharing its validation:
    /// these cues carry no WebVTT metadata, and accepting any would mean a
    /// converter had invented it.
    Text,
}

impl CueDialect {
    /// The dialect for `codec`, or `None` when it cannot be presented as WebVTT.
    pub(super) fn for_codec(codec: Codec) -> Option<Self> {
        match codec {
            Codec::WebVtt => Some(Self::PassThrough),
            Codec::SubRip => Some(Self::SubRip),
            Codec::Text => Some(Self::Text),
            _ => None,
        }
    }

    /// Converts one cue, rejecting anything WebVTT cannot carry faithfully.
    ///
    /// Purely a function of the cue: nothing here observes segmentation state,
    /// so a rejection leaves the caller's queue untouched.
    pub(super) fn read(self, sample: &SubtitleSample) -> Result<CueAction, MuxError> {
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
            Self::PassThrough => Ok(CueAction::Show(CueContent {
                metadata: validate_metadata(sample.webvtt.clone())?,
                text: webvtt_text(payload)?,
            })),
            Self::SubRip => {
                if sample.webvtt != WebVttCueMetadata::default() {
                    return Err(mux_error(format!(
                        "{} SubRip cue carries WebVTT-only metadata",
                        sample.track_id
                    )));
                }
                Ok(CueAction::Show(CueContent {
                    metadata: WebVttCueMetadata::default(),
                    text: subrip::convert(payload)?,
                }))
            }
            // Already the text a cue body carries, so the WebVTT reader is the
            // whole conversion: it is the same UTF-8, blank-line and NUL check
            // every dialect has to pass before it can be rendered.
            Self::Text => {
                if sample.webvtt != WebVttCueMetadata::default() {
                    return Err(mux_error(format!(
                        "{} text cue carries WebVTT-only metadata",
                        sample.track_id
                    )));
                }
                // An empty body is the one payload this dialect reads as an
                // instruction rather than as content. FLV script data has no
                // erase message, so a publisher that wants to stop displaying a
                // caption can only send a cue that renders as nothing — and
                // WebVTT cannot carry that, since a blank body terminates a
                // cue. Reading it as a clear is what lets the publisher keep
                // ownership of when its captions disappear.
                //
                // Every other empty-payload rule is unchanged: this is not a
                // relaxation of the WebVTT validation below, which still
                // rejects an empty body for the dialects that have a real end.
                if payload.is_empty() {
                    return Ok(CueAction::Clear);
                }
                Ok(CueAction::Show(CueContent {
                    metadata: WebVttCueMetadata::default(),
                    text: webvtt_text(payload)?,
                }))
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

    /// The content of a cue that displays something, or a failure if the
    /// dialect read it as a clear.
    fn shown(action: CueAction) -> CueContent {
        match action {
            CueAction::Show(content) => content,
            CueAction::Clear => panic!("expected a displayable cue, got a clear"),
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
        assert_eq!(CueDialect::for_codec(Codec::Text), Some(CueDialect::Text));
        assert_eq!(CueDialect::for_codec(Codec::MovText), None);
    }

    #[test]
    fn text_cues_convert_as_plain_utf8_and_refuse_webvtt_metadata()
    -> Result<(), Box<dyn std::error::Error>> {
        // FLV script data carries the cue body and nothing else, so conversion
        // is the shared WebVTT validation rather than a markup translation.
        let content = shown(CueDialect::Text.read(&sample(Codec::Text, "hej världen".as_bytes()))?);
        assert_eq!(content.text.as_ref(), "hej världen");
        assert_eq!(content.metadata, WebVttCueMetadata::default());

        // A converter that accepted these would be inventing them: nothing in
        // the wire format can carry a cue identifier or settings.
        let mut carries_metadata = sample(Codec::Text, b"text");
        carries_metadata.webvtt = WebVttCueMetadata {
            identifier: Some(Arc::from("cue-one")),
            settings: None,
        };
        assert!(CueDialect::Text.read(&carries_metadata).is_err());
        Ok(())
    }

    #[test]
    fn a_text_cue_is_held_to_the_same_payload_rules_as_webvtt() {
        for payload in [
            b"\xff\xfe invalid utf8".as_slice(),
            b"has\0nul".as_slice(),
            // A blank line ends a cue, so a body containing one would silently
            // become two cues sharing a timing line.
            b"first\n\nsecond".as_slice(),
        ] {
            assert!(CueDialect::Text.read(&sample(Codec::Text, payload)).is_err());
        }
    }

    #[test]
    fn an_empty_text_cue_clears_the_display_rather_than_failing() {
        // The one payload this dialect reads as an instruction. FLV script
        // data has no erase message, so a publisher can only say "show
        // nothing" with an empty body, and WebVTT cannot carry that as a cue.
        assert!(matches!(
            CueDialect::Text.read(&sample(Codec::Text, b"")),
            Ok(CueAction::Clear)
        ));
    }

    #[test]
    fn an_empty_body_still_fails_the_dialects_that_carry_their_own_end() {
        // Only open-ended cues need a clear signal. A format that states when
        // its cue ends has no use for one, so an empty body there is a
        // malformed cue rather than an instruction.
        for (dialect, codec) in [
            (CueDialect::PassThrough, Codec::WebVtt),
            (CueDialect::SubRip, Codec::SubRip),
        ] {
            assert!(dialect.read(&sample(codec, b"")).is_err());
        }
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
        let content = shown(CueDialect::SubRip.read(&sample(Codec::SubRip, b"<B>bold</B>"))?);
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
        for dialect in [CueDialect::PassThrough, CueDialect::SubRip, CueDialect::Text] {
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
