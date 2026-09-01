use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use rml_amf0::Amf0Value;
use thiserror::Error;

use crate::{EnhancedValidationMode, MediaInterpretation};

/// A typed `onMetaData` view plus the exact encoded AMF payload.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedMetadata {
    /// Original RTMP message body, authoritative for forwarding and demuxing.
    pub raw: Bytes,
    pub interpretation: MediaInterpretation<ParsedMetadata>,
}

/// Owned v2 r2 metadata, including descriptors for non-default tracks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedMetadata {
    /// Every top-level property, including values unknown to this crate.
    pub properties: HashMap<String, Amf0Value>,
    pub audio_tracks: BTreeMap<u32, TrackMetadata>,
    pub video_tracks: BTreeMap<u32, TrackMetadata>,
}

/// Metadata for one non-default track. Unknown fields remain in `properties`.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackMetadata {
    pub track_id: u32,
    pub codec: Option<MetadataCodec>,
    pub properties: HashMap<String, Amf0Value>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MetadataCodec {
    /// Legacy FLV numeric codec identifier.
    Legacy(f64),
    /// Big-endian four-byte codec identifier used by Enhanced RTMP.
    FourCc([u8; 4]),
}

/// The encoder-declared shape of a publication, read from the default-track
/// `onMetaData` properties.
///
/// Every field is optional: `onMetaData` is advisory, encoders disagree about
/// which properties they send, and some omit the message entirely. Callers must
/// treat a missing field as "unknown" rather than as a fault.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncoderSummary {
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Declared framerate. This is what the encoder intends to send, not what
    /// it actually delivered; measured rates must come from frame counters.
    pub framerate: Option<f64>,
}

impl EncoderSummary {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl ParsedMetadata {
    /// Summarize the default track's encoder properties for observability.
    pub fn encoder_summary(&self) -> EncoderSummary {
        EncoderSummary {
            video_codec: self.codec_name("videocodecid", TrackKind::Video),
            audio_codec: self.codec_name("audiocodecid", TrackKind::Audio),
            width: self.dimension("width"),
            height: self.dimension("height"),
            framerate: self
                .bounded_positive_number("framerate", MAX_DECLARED_FRAMERATE)
                .map(|value| (value * 1_000.0).round() / 1_000.0),
        }
    }

    fn codec_name(&self, field: &str, kind: TrackKind) -> Option<String> {
        match self.properties.get(field)? {
            Amf0Value::Number(value) => codec_label(*value, kind),
            // Enhanced RTMP encoders may send the FourCC as a string directly.
            // Only retain a known value: metadata becomes a Prometheus label,
            // so arbitrary publisher-controlled strings would create unbounded
            // series churn when onMetaData is resent.
            Amf0Value::Utf8String(value) => {
                let bytes: [u8; 4] = value.trim().as_bytes().try_into().ok()?;
                known_fourcc(bytes, kind).then(|| String::from_utf8_lossy(&bytes).into_owned())
            }
            _ => None,
        }
    }

    fn dimension(&self, field: &str) -> Option<u32> {
        let value = self.positive_number(field)?;
        // Guard against absurd declarations; a real frame dimension fits well
        // inside u16 and a fractional or bogus one must not become a label.
        (value.fract() == 0.0 && value <= f64::from(u16::MAX)).then_some(value as u32)
    }

    fn positive_number(&self, field: &str) -> Option<f64> {
        match self.properties.get(field)? {
            Amf0Value::Number(value) if value.is_finite() && *value > 0.0 => Some(*value),
            _ => None,
        }
    }

    fn bounded_positive_number(&self, field: &str, maximum: f64) -> Option<f64> {
        self.positive_number(field)
            .filter(|value| *value <= maximum)
    }
}

/// Map an `onMetaData` codec id to a display label.
///
/// Enhanced RTMP encodes the id as a big-endian FourCC; legacy FLV uses a small
/// integer. The two ranges do not overlap, so the byte pattern decides.
fn codec_label(value: f64, kind: TrackKind) -> Option<String> {
    if !value.is_finite() || value.fract() != 0.0 || !(0.0..=f64::from(u32::MAX)).contains(&value) {
        return None;
    }
    let id = value as u32;
    let bytes = id.to_be_bytes();
    if bytes.iter().all(u8::is_ascii_graphic) {
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    let label = match kind {
        TrackKind::Video => match id {
            2 => "h263",
            3 => "screen",
            4 => "vp6",
            5 => "vp6a",
            6 => "screen2",
            7 => "avc1",
            12 => "hvc1",
            13 => "av01",
            _ => return None,
        },
        TrackKind::Audio => match id {
            0 => "pcm",
            1 => "adpcm",
            2 => "mp3",
            4..=6 => "nellymoser",
            10 => "mp4a",
            11 => "speex",
            _ => return None,
        },
    };
    Some(label.to_owned())
}

#[derive(Debug, Error)]
#[error("malformed Enhanced RTMP metadata: {reason}")]
pub struct MetadataValidationError {
    reason: String,
}

impl ValidatedMetadata {
    pub fn parse(
        raw: Bytes,
        values: Vec<(String, Amf0Value)>,
        mode: EnhancedValidationMode,
    ) -> Result<Self, MetadataValidationError> {
        let properties = values.into_iter().collect::<HashMap<_, _>>();
        match parse_metadata(properties) {
            Ok(parsed) => Ok(Self {
                raw,
                interpretation: MediaInterpretation::Parsed(parsed),
            }),
            Err(reason) if mode == EnhancedValidationMode::Passthrough => Ok(Self {
                raw,
                interpretation: MediaInterpretation::Opaque { reason },
            }),
            Err(reason) => Err(MetadataValidationError { reason }),
        }
    }
}

#[derive(Clone, Copy)]
enum TrackKind {
    Audio,
    Video,
}

const MAX_DECLARED_FRAMERATE: f64 = 1_000.0;
const AUDIO_FOURCCS: [[u8; 4]; 6] = [*b"ac-3", *b"ec-3", *b"Opus", *b".mp3", *b"fLaC", *b"mp4a"];
const VIDEO_FOURCCS: [[u8; 4]; 6] = [*b"vp08", *b"vp09", *b"av01", *b"avc1", *b"hvc1", *b"vvc1"];

fn known_fourcc(value: [u8; 4], kind: TrackKind) -> bool {
    match kind {
        TrackKind::Audio => AUDIO_FOURCCS.contains(&value),
        TrackKind::Video => VIDEO_FOURCCS.contains(&value),
    }
}

fn parse_metadata(properties: HashMap<String, Amf0Value>) -> Result<ParsedMetadata, String> {
    let audio_tracks = parse_track_map(
        properties.get("audioTrackIdInfoMap"),
        "audioTrackIdInfoMap",
        TrackKind::Audio,
    )?;
    let video_tracks = parse_track_map(
        properties.get("videoTrackIdInfoMap"),
        "videoTrackIdInfoMap",
        TrackKind::Video,
    )?;
    Ok(ParsedMetadata {
        properties,
        audio_tracks,
        video_tracks,
    })
}

fn parse_track_map(
    value: Option<&Amf0Value>,
    field: &str,
    kind: TrackKind,
) -> Result<BTreeMap<u32, TrackMetadata>, String> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Amf0Value::Object(entries) = value else {
        return Err(format!("{field} must be an object"));
    };
    entries
        .iter()
        .map(|(track_id, value)| {
            let track_id = track_id
                .parse::<u32>()
                .map_err(|_| format!("{field} contains non-numeric track id {track_id:?}"))?;
            if track_id == 0 {
                return Err(format!("{field} must not describe default track 0"));
            }
            let Amf0Value::Object(properties) = value else {
                return Err(format!("{field}[{track_id}] must be an object"));
            };
            let codec_field = match kind {
                TrackKind::Audio => "audiocodecid",
                TrackKind::Video => "videocodecid",
            };
            let codec = properties
                .get(codec_field)
                .map(|value| parse_codec(value, kind, field, track_id))
                .transpose()?;
            Ok((
                track_id,
                TrackMetadata {
                    track_id,
                    codec,
                    properties: properties.clone(),
                },
            ))
        })
        .collect()
}

fn parse_codec(
    value: &Amf0Value,
    kind: TrackKind,
    field: &str,
    track_id: u32,
) -> Result<MetadataCodec, String> {
    let Amf0Value::Number(value) = value else {
        return Err(format!("{field}[{track_id}] codec id must be numeric"));
    };
    if !value.is_finite() || value.fract() != 0.0 || !(0.0..=f64::from(u32::MAX)).contains(value) {
        return Err(format!("{field}[{track_id}] codec id is invalid"));
    }
    let bytes = (*value as u32).to_be_bytes();
    if !bytes.iter().all(u8::is_ascii_graphic) {
        return Ok(MetadataCodec::Legacy(*value));
    }
    if !known_fourcc(bytes, kind) {
        return Err(format!(
            "{field}[{track_id}] has unknown FourCC {:?}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    Ok(MetadataCodec::FourCc(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn four_cc(value: &[u8; 4]) -> Amf0Value {
        Amf0Value::Number(f64::from(u32::from_be_bytes(*value)))
    }

    #[test]
    fn parses_v2_track_maps_and_preserves_unknown_fields() {
        let raw = Bytes::from_static(b"encoded metadata");
        let values = vec![
            (
                "videoTrackIdInfoMap".into(),
                Amf0Value::Object(HashMap::from([(
                    "1".into(),
                    Amf0Value::Object(HashMap::from([
                        ("videocodecid".into(), four_cc(b"vvc1")),
                        ("vendorHint".into(), Amf0Value::Boolean(true)),
                    ])),
                )])),
            ),
            (
                "audioTrackIdInfoMap".into(),
                Amf0Value::Object(HashMap::from([(
                    "2".into(),
                    Amf0Value::Object(HashMap::from([("audiocodecid".into(), four_cc(b"Opus"))])),
                )])),
            ),
        ];
        let metadata =
            ValidatedMetadata::parse(raw.clone(), values, EnhancedValidationMode::Strict)
                .expect("valid track maps parse");
        assert_eq!(metadata.raw, raw);
        let MediaInterpretation::Parsed(metadata) = metadata.interpretation else {
            panic!("valid metadata must be typed");
        };
        assert_eq!(
            metadata.video_tracks[&1].codec,
            Some(MetadataCodec::FourCc(*b"vvc1"))
        );
        assert_eq!(
            metadata.audio_tracks[&2].codec,
            Some(MetadataCodec::FourCc(*b"Opus"))
        );
        assert_eq!(
            metadata.video_tracks[&1].properties.get("vendorHint"),
            Some(&Amf0Value::Boolean(true))
        );
    }

    #[test]
    fn malformed_track_maps_are_strict_or_opaque() {
        let raw = Bytes::from_static(b"raw");
        let values = vec![(
            "videoTrackIdInfoMap".into(),
            Amf0Value::Object(HashMap::from([(
                "0".into(),
                Amf0Value::Object(HashMap::new()),
            )])),
        )];
        assert!(
            ValidatedMetadata::parse(raw.clone(), values.clone(), EnhancedValidationMode::Strict)
                .is_err()
        );
        let metadata =
            ValidatedMetadata::parse(raw.clone(), values, EnhancedValidationMode::Passthrough)
                .expect("passthrough keeps malformed metadata");
        assert_eq!(metadata.raw, raw);
        assert!(matches!(
            metadata.interpretation,
            MediaInterpretation::Opaque { .. }
        ));
    }

    #[test]
    fn encoder_summary_only_exposes_bounded_canonical_labels() {
        let metadata = ParsedMetadata {
            properties: HashMap::from([
                ("videocodecid".into(), Amf0Value::Utf8String("avc1".into())),
                ("audiocodecid".into(), Amf0Value::Utf8String("mp4a".into())),
                ("width".into(), Amf0Value::Number(1920.0)),
                ("height".into(), Amf0Value::Number(1080.0)),
                ("framerate".into(), Amf0Value::Number(29.970_029)),
            ]),
            ..Default::default()
        };
        assert_eq!(
            metadata.encoder_summary(),
            EncoderSummary {
                video_codec: Some("avc1".into()),
                audio_codec: Some("mp4a".into()),
                width: Some(1920),
                height: Some(1080),
                framerate: Some(29.97),
            }
        );

        let hostile = ParsedMetadata {
            properties: HashMap::from([
                (
                    "videocodecid".into(),
                    Amf0Value::Utf8String("attacker-controlled-codec".into()),
                ),
                ("audiocodecid".into(), Amf0Value::Utf8String("nope".into())),
                ("width".into(), Amf0Value::Number(1920.5)),
                ("height".into(), Amf0Value::Number(1_000_000.0)),
                ("framerate".into(), Amf0Value::Number(1_001.0)),
            ]),
            ..Default::default()
        };
        assert!(hostile.encoder_summary().is_empty());
    }
}
