use std::io::Cursor;

use bytes::Bytes;
use scuffle_flv::{
    audio::{
        AudioData,
        body::{
            AudioTagBody,
            enhanced::{AudioPacket, ExAudioTagBody},
            legacy::{LegacyAudioTagBody, aac::AacAudioData},
        },
        header::{
            AudioTagHeader,
            enhanced::{AudioPacketModEx, ExAudioTagHeaderContent},
        },
    },
    video::{
        VideoData,
        body::{
            VideoTagBody,
            enhanced::{ExVideoTagBody, VideoPacket},
            legacy::LegacyVideoTagBody,
        },
        header::{
            VideoFrameType, VideoTagHeaderData,
            enhanced::{ExVideoTagHeaderContent, VideoPacketModEx},
            legacy::{LegacyVideoTagHeader, LegacyVideoTagHeaderAvcPacket},
        },
    },
};
use thiserror::Error;

use crate::EnhancedValidationMode;

/// A raw media payload plus its typed FLV interpretation.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedMedia<T> {
    /// Original RTMP message body. This is authoritative for republishing.
    pub raw: Bytes,
    /// Parsed interpretation, or an opaque reason in passthrough mode.
    pub interpretation: MediaInterpretation<T>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaInterpretation<T> {
    Parsed(T),
    Opaque { reason: String },
}

/// Owned typed audio interpretation from `scuffle-flv`.
pub type ParsedAudio = AudioData;
/// Owned typed video interpretation from `scuffle-flv`.
pub type ParsedVideo = VideoData<'static>;

/// Media facts needed by relays without exposing FLV parser internals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaClassification {
    /// The message contains one or more coded media frames.
    pub coded: bool,
    /// The message carries a codec configuration/sequence header.
    pub configuration: bool,
    /// A coded video message is marked as a keyframe.
    pub keyframe: bool,
}

#[derive(Debug, Error)]
pub enum MediaValidationError {
    #[error("malformed Enhanced/legacy FLV {kind} payload: {reason}")]
    Malformed { kind: &'static str, reason: String },
}

impl ValidatedMedia<ParsedAudio> {
    pub fn parse_audio(
        raw: Bytes,
        mode: EnhancedValidationMode,
    ) -> Result<Self, MediaValidationError> {
        let mut reader = Cursor::new(raw.clone());
        match AudioData::demux(&mut reader) {
            Ok(parsed) => finish_audio(raw, parsed, mode),
            Err(error) if mode == EnhancedValidationMode::Passthrough => Ok(Self {
                raw,
                interpretation: MediaInterpretation::Opaque {
                    reason: error.to_string(),
                },
            }),
            Err(error) => Err(MediaValidationError::Malformed {
                kind: "audio",
                reason: error.to_string(),
            }),
        }
    }

    /// Classify a validated audio message. Opaque passthrough messages are
    /// deliberately left unclassified rather than guessed from raw bytes.
    pub fn classification(&self) -> MediaClassification {
        let MediaInterpretation::Parsed(parsed) = &self.interpretation else {
            return MediaClassification::default();
        };
        match &parsed.body {
            AudioTagBody::Legacy(LegacyAudioTagBody::Aac(AacAudioData::SequenceHeader(_))) => {
                MediaClassification {
                    configuration: true,
                    ..Default::default()
                }
            }
            AudioTagBody::Legacy(LegacyAudioTagBody::Aac(AacAudioData::Raw(_)))
            | AudioTagBody::Legacy(LegacyAudioTagBody::Other { .. }) => MediaClassification {
                coded: true,
                ..Default::default()
            },
            AudioTagBody::Enhanced(body) => classify_enhanced_audio(body),
            AudioTagBody::Legacy(LegacyAudioTagBody::Aac(AacAudioData::Unknown { .. })) => {
                MediaClassification::default()
            }
        }
    }
}

impl ValidatedMedia<ParsedVideo> {
    pub fn parse_video(
        raw: Bytes,
        mode: EnhancedValidationMode,
    ) -> Result<Self, MediaValidationError> {
        let mut reader = Cursor::new(raw.clone());
        match VideoData::demux(&mut reader) {
            Ok(parsed) => finish_video(raw, parsed, mode),
            Err(error) if mode == EnhancedValidationMode::Passthrough => Ok(Self {
                raw,
                interpretation: MediaInterpretation::Opaque {
                    reason: error.to_string(),
                },
            }),
            Err(error) => Err(MediaValidationError::Malformed {
                kind: "video",
                reason: error.to_string(),
            }),
        }
    }

    /// Classify a validated video message across legacy and Enhanced RTMP.
    pub fn classification(&self) -> MediaClassification {
        let MediaInterpretation::Parsed(parsed) = &self.interpretation else {
            return MediaClassification::default();
        };
        let keyframe = parsed.header.frame_type == VideoFrameType::KeyFrame;
        match (&parsed.header.data, &parsed.body) {
            (
                VideoTagHeaderData::Legacy(LegacyVideoTagHeader::AvcPacket(
                    LegacyVideoTagHeaderAvcPacket::SequenceHeader,
                )),
                _,
            ) => MediaClassification {
                configuration: true,
                ..Default::default()
            },
            (
                VideoTagHeaderData::Legacy(LegacyVideoTagHeader::AvcPacket(
                    LegacyVideoTagHeaderAvcPacket::Nalu { .. },
                )),
                _,
            ) => MediaClassification {
                coded: true,
                keyframe,
                configuration: false,
            },
            (VideoTagHeaderData::Legacy(LegacyVideoTagHeader::AvcPacket(_)), _) => {
                MediaClassification::default()
            }
            (VideoTagHeaderData::Legacy(LegacyVideoTagHeader::VideoCommand(_)), _)
            | (_, VideoTagBody::Legacy(LegacyVideoTagBody::Command)) => {
                MediaClassification::default()
            }
            (_, VideoTagBody::Legacy(LegacyVideoTagBody::AvcVideoPacketSeqHdr(_))) => {
                MediaClassification {
                    configuration: true,
                    ..Default::default()
                }
            }
            (_, VideoTagBody::Legacy(LegacyVideoTagBody::Other { .. })) => MediaClassification {
                coded: true,
                keyframe,
                configuration: false,
            },
            (_, VideoTagBody::Enhanced(body)) => classify_enhanced_video(body, keyframe),
        }
    }
}

fn classify_enhanced_audio(body: &ExAudioTagBody) -> MediaClassification {
    let mut classification = MediaClassification::default();
    let mut classify = |packet: &AudioPacket| match packet {
        AudioPacket::SequenceStart { .. } => classification.configuration = true,
        AudioPacket::CodedFrames { .. } => classification.coded = true,
        _ => {}
    };
    match body {
        ExAudioTagBody::NoMultitrack { packet, .. } => classify(packet),
        ExAudioTagBody::ManyTracks(tracks) => {
            for track in tracks {
                classify(&track.packet);
            }
        }
    }
    classification
}

fn classify_enhanced_video(body: &ExVideoTagBody<'_>, keyframe: bool) -> MediaClassification {
    let mut classification = MediaClassification::default();
    let mut classify = |packet: &VideoPacket<'_>| match packet {
        VideoPacket::SequenceStart(_) | VideoPacket::Mpeg2TsSequenceStart(_) => {
            classification.configuration = true;
        }
        VideoPacket::CodedFrames(_) | VideoPacket::CodedFramesX { .. } => {
            classification.coded = true;
            classification.keyframe |= keyframe;
        }
        _ => {}
    };
    match body {
        ExVideoTagBody::NoMultitrack { packet, .. } => classify(packet),
        ExVideoTagBody::ManyTracks(tracks) => {
            for track in tracks {
                classify(&track.packet);
            }
        }
        ExVideoTagBody::Command => {}
    }
    classification
}

fn finish_audio(
    raw: Bytes,
    parsed: ParsedAudio,
    mode: EnhancedValidationMode,
) -> Result<ValidatedMedia<ParsedAudio>, MediaValidationError> {
    match validate_audio_unknowns(&parsed) {
        Ok(()) => Ok(ValidatedMedia {
            raw,
            interpretation: MediaInterpretation::Parsed(parsed),
        }),
        Err(reason) if mode == EnhancedValidationMode::Passthrough => Ok(ValidatedMedia {
            raw,
            interpretation: MediaInterpretation::Opaque { reason },
        }),
        Err(reason) => Err(MediaValidationError::Malformed {
            kind: "audio",
            reason,
        }),
    }
}

fn finish_video(
    raw: Bytes,
    parsed: ParsedVideo,
    mode: EnhancedValidationMode,
) -> Result<ValidatedMedia<ParsedVideo>, MediaValidationError> {
    match validate_video_unknowns(&parsed) {
        Ok(()) => Ok(ValidatedMedia {
            raw,
            interpretation: MediaInterpretation::Parsed(parsed),
        }),
        Err(reason) if mode == EnhancedValidationMode::Passthrough => Ok(ValidatedMedia {
            raw,
            interpretation: MediaInterpretation::Opaque { reason },
        }),
        Err(reason) => Err(MediaValidationError::Malformed {
            kind: "video",
            reason,
        }),
    }
}

fn validate_audio_unknowns(parsed: &ParsedAudio) -> Result<(), String> {
    let AudioTagHeader::Enhanced(header) = &parsed.header else {
        return Ok(());
    };
    if header
        .audio_packet_mod_exs
        .iter()
        .any(|value| matches!(value, AudioPacketModEx::Other { .. }))
    {
        return Err("unknown audio ModEx type".to_owned());
    }
    if matches!(header.content, ExAudioTagHeaderContent::Unknown { .. }) {
        return Err("unknown audio multitrack type".to_owned());
    }
    let AudioTagBody::Enhanced(body) = &parsed.body else {
        return Ok(());
    };
    match body {
        ExAudioTagBody::NoMultitrack {
            audio_four_cc,
            packet,
        } => validate_audio_track(audio_four_cc.0, packet),
        ExAudioTagBody::ManyTracks(tracks) => tracks
            .iter()
            .try_for_each(|track| validate_audio_track(track.audio_four_cc.0, &track.packet)),
    }
}

fn validate_audio_track(four_cc: [u8; 4], packet: &AudioPacket) -> Result<(), String> {
    const KNOWN: [[u8; 4]; 6] = [*b"ac-3", *b"ec-3", *b"Opus", *b".mp3", *b"fLaC", *b"mp4a"];
    if !KNOWN.contains(&four_cc) {
        return Err(format!(
            "unknown audio FourCC {:?}",
            String::from_utf8_lossy(&four_cc)
        ));
    }
    if matches!(packet, AudioPacket::Unknown { .. }) {
        return Err("unknown audio packet type".to_owned());
    }
    Ok(())
}

fn validate_video_unknowns(parsed: &ParsedVideo) -> Result<(), String> {
    let VideoTagHeaderData::Enhanced(header) = &parsed.header.data else {
        return Ok(());
    };
    if header
        .video_packet_mod_exs
        .iter()
        .any(|value| matches!(value, VideoPacketModEx::Other { .. }))
    {
        return Err("unknown video ModEx type".to_owned());
    }
    if matches!(header.content, ExVideoTagHeaderContent::Unknown { .. }) {
        return Err("unknown video multitrack type".to_owned());
    }
    let VideoTagBody::Enhanced(body) = &parsed.body else {
        return Ok(());
    };
    match body {
        ExVideoTagBody::Command => Ok(()),
        ExVideoTagBody::NoMultitrack {
            video_four_cc,
            packet,
        } => validate_video_track(video_four_cc.0, packet),
        ExVideoTagBody::ManyTracks(tracks) => tracks
            .iter()
            .try_for_each(|track| validate_video_track(track.video_four_cc.0, &track.packet)),
    }
}

fn validate_video_track(four_cc: [u8; 4], packet: &VideoPacket<'_>) -> Result<(), String> {
    // v2 r2 adds VVC (`vvc1`) beyond scuffle-flv 0.2.2's named constants.
    const KNOWN: [[u8; 4]; 6] = [*b"vp08", *b"vp09", *b"av01", *b"avc1", *b"hvc1", *b"vvc1"];
    if !KNOWN.contains(&four_cc) {
        return Err(format!(
            "unknown video FourCC {:?}",
            String::from_utf8_lossy(&four_cc)
        ));
    }
    if matches!(packet, VideoPacket::Unknown { .. }) {
        return Err("unknown video packet type".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_enhanced_video_is_strict_by_default_and_opaque_in_passthrough() {
        // Enhanced flag + SequenceStart, but no required FourCC.
        let raw = Bytes::from_static(&[0x90]);
        assert!(ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Strict).is_err());
        let media =
            ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Passthrough).unwrap();
        assert_eq!(media.raw, raw);
        assert!(matches!(
            media.interpretation,
            MediaInterpretation::Opaque { .. }
        ));
    }

    #[test]
    fn parses_vvc_as_an_owned_unknown_fourcc_model() {
        // Key frame + enhanced SequenceEnd + VVC FourCC. SequenceEnd has no body.
        let raw = Bytes::from_static(b"\x92vvc1");
        let media =
            ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Strict).unwrap();
        assert_eq!(media.raw, raw);
        assert!(matches!(
            media.interpretation,
            MediaInterpretation::Parsed(_)
        ));
    }

    #[test]
    fn unknown_fourcc_is_rejected_or_retained_opaque() {
        let raw = Bytes::from_static(b"\x92zzzz");
        assert!(ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Strict).is_err());
        let media =
            ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Passthrough).unwrap();
        assert_eq!(media.raw, raw);
        assert!(matches!(
            media.interpretation,
            MediaInterpretation::Opaque { .. }
        ));
    }

    #[test]
    fn unknown_modex_is_rejected_or_retained_opaque() {
        let raw = Bytes::from_static(b"\x97\x00\x2a\x12hvc1");
        assert!(ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Strict).is_err());
        assert!(matches!(
            ValidatedMedia::parse_video(raw, EnhancedValidationMode::Passthrough)
                .unwrap()
                .interpretation,
            MediaInterpretation::Opaque { .. }
        ));
    }

    #[test]
    fn parses_video_and_audio_multitrack_packets() {
        let video = Bytes::from_static(b"\x96\x02hvc1\x00");
        assert!(ValidatedMedia::parse_video(video, EnhancedValidationMode::Strict).is_ok());
        let audio = Bytes::from_static(b"\x95\x02Opus\x00");
        assert!(ValidatedMedia::parse_audio(audio, EnhancedValidationMode::Strict).is_ok());
    }

    #[test]
    fn parses_multichannel_audio_configuration() {
        let audio = Bytes::from_static(b"\x94mp4a\x00\x02");
        assert!(ValidatedMedia::parse_audio(audio, EnhancedValidationMode::Strict).is_ok());
    }

    #[test]
    fn parses_every_enhanced_video_packet_family() {
        for raw in [
            b"\x90vp08config".as_slice(),
            b"\x91av01frame".as_slice(),
            b"\x92hvc1".as_slice(),
            b"\x93av01frame".as_slice(),
            b"\x94av01".as_slice(),
            b"\x95vp08descriptor".as_slice(),
            b"\x96\x02hvc1\x00".as_slice(),
            b"\x97\x02\x00\x00\x01\x02hvc1".as_slice(),
        ] {
            ValidatedMedia::parse_video(
                Bytes::copy_from_slice(raw),
                EnhancedValidationMode::Strict,
            )
            .unwrap_or_else(|error| panic!("valid video family {raw:?} failed: {error}"));
        }
    }

    #[test]
    fn parses_every_enhanced_audio_packet_family() {
        for raw in [
            b"\x90Opusconfig".as_slice(),
            b"\x91Opusframe".as_slice(),
            b"\x92Opus".as_slice(),
            b"\x94mp4a\x00\x02".as_slice(),
            b"\x95\x02Opus\x00".as_slice(),
            b"\x97\x02\x00\x00\x01\x02Opus".as_slice(),
        ] {
            ValidatedMedia::parse_audio(
                Bytes::copy_from_slice(raw),
                EnhancedValidationMode::Strict,
            )
            .unwrap_or_else(|error| panic!("valid audio family {raw:?} failed: {error}"));
        }
    }

    #[test]
    fn unknown_audio_values_are_rejected_or_retained_opaque() {
        for raw in [
            b"\x92zzzz".as_slice(),
            b"\x93Opus".as_slice(),
            b"\x97\x00\x2a\x12Opus".as_slice(),
        ] {
            let raw = Bytes::copy_from_slice(raw);
            assert!(
                ValidatedMedia::parse_audio(raw.clone(), EnhancedValidationMode::Strict).is_err()
            );
            assert!(matches!(
                ValidatedMedia::parse_audio(raw, EnhancedValidationMode::Passthrough)
                    .unwrap()
                    .interpretation,
                MediaInterpretation::Opaque { .. }
            ));
        }
    }

    #[test]
    fn unknown_video_packet_type_is_rejected_or_retained_opaque() {
        let raw = Bytes::from_static(b"\x98hvc1");
        assert!(ValidatedMedia::parse_video(raw.clone(), EnhancedValidationMode::Strict).is_err());
        assert!(matches!(
            ValidatedMedia::parse_video(raw, EnhancedValidationMode::Passthrough)
                .unwrap()
                .interpretation,
            MediaInterpretation::Opaque { .. }
        ));
    }

    #[test]
    fn classifies_enhanced_configurations_coded_frames_and_keyframes() {
        let video_config = ValidatedMedia::parse_video(
            Bytes::from_static(b"\x90vp08config"),
            EnhancedValidationMode::Strict,
        )
        .unwrap();
        assert_eq!(
            video_config.classification(),
            MediaClassification {
                configuration: true,
                ..Default::default()
            }
        );

        let video_frame = ValidatedMedia::parse_video(
            Bytes::from_static(b"\x91vp08frame"),
            EnhancedValidationMode::Strict,
        )
        .unwrap();
        assert_eq!(
            video_frame.classification(),
            MediaClassification {
                coded: true,
                keyframe: true,
                configuration: false
            }
        );

        let audio_config = ValidatedMedia::parse_audio(
            Bytes::from_static(b"\x90Opusconfig"),
            EnhancedValidationMode::Strict,
        )
        .unwrap();
        assert!(audio_config.classification().configuration);
        let audio_frame = ValidatedMedia::parse_audio(
            Bytes::from_static(b"\x91Opusframe"),
            EnhancedValidationMode::Strict,
        )
        .unwrap();
        assert!(audio_frame.classification().coded);
    }
}
