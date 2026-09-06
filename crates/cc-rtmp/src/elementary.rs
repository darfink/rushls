//! Elementary access units from validated RTMP media.
//!
//! `scuffle-flv` stays inside this crate. Callers receive length-prefixed video,
//! raw AAC or Opus, and decoder-configuration records (`avcC` / `hvcC` / `av1C` /
//! AudioSpecificConfig / OpusHead) without seeing Annex-B, ADTS, or FLV tag layout.

use bytes::Bytes;
use scuffle_flv::{
    audio::{
        body::{
            AudioTagBody,
            enhanced::{AudioPacket, ExAudioTagBody},
            legacy::{LegacyAudioTagBody, aac::AacAudioData},
        },
        header::enhanced::AudioFourCc,
    },
    video::{
        body::{
            VideoTagBody,
            enhanced::{
                ExVideoTagBody, VideoPacket, VideoPacketCodedFrames, VideoPacketSequenceStart,
            },
            legacy::LegacyVideoTagBody,
        },
        header::{
            VideoFrameType, VideoTagHeaderData,
            enhanced::VideoFourCc,
            legacy::{LegacyVideoTagHeader, LegacyVideoTagHeaderAvcPacket},
        },
    },
};

use crate::{
    MediaInterpretation, ParsedAudio, ParsedVideo, ValidatedMedia, media::MediaValidationError,
};

/// Codecs this ingest path can present as elementary access units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElementaryCodec {
    Avc,
    Hevc,
    Av1,
    Aac,
    Opus,
}

impl ElementaryCodec {
    pub fn is_video(self) -> bool {
        matches!(self, Self::Avc | Self::Hevc | Self::Av1)
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Self::Aac | Self::Opus)
    }
}

/// One validated RTMP message, reduced to decoder config or a coded sample.
#[derive(Clone, Debug, PartialEq)]
pub enum ElementaryUnit {
    Configuration {
        codec: ElementaryCodec,
        extradata: Bytes,
        /// Enhanced RTMP track id when the message names one; legacy is `None`.
        track_id: Option<u8>,
    },
    Sample {
        codec: ElementaryCodec,
        payload: Bytes,
        keyframe: bool,
        /// Composition offset in milliseconds on the RTMP clock. Audio is 0.
        composition_time_offset: i32,
        track_id: Option<u8>,
    },
}

impl ElementaryUnit {
    pub fn codec(&self) -> ElementaryCodec {
        match self {
            Self::Configuration { codec, .. } | Self::Sample { codec, .. } => *codec,
        }
    }

    /// Enhanced RTMP track id when the message names one; legacy is `None`.
    pub fn track_id(&self) -> Option<u8> {
        match self {
            Self::Configuration { track_id, .. } | Self::Sample { track_id, .. } => *track_id,
        }
    }
}

impl ValidatedMedia<ParsedAudio> {
    /// Maps a validated audio message onto elementary units.
    ///
    /// Unmapped codecs (MP3, AC-3, …) and sequence-end / channel-config
    /// signalling yield an empty list so the session can ignore them. One
    /// Enhanced tag may carry several tracks; each mapped track is its own
    /// unit so a packed `ManyTracks` message does not drop siblings.
    pub fn elementary_units(&self) -> Result<Vec<ElementaryUnit>, MediaValidationError> {
        match &self.interpretation {
            MediaInterpretation::Opaque { reason } => Err(MediaValidationError::Malformed {
                kind: "audio",
                reason: reason.clone(),
            }),
            MediaInterpretation::Parsed(parsed) => match &parsed.body {
                AudioTagBody::Legacy(LegacyAudioTagBody::Aac(AacAudioData::SequenceHeader(_))) => {
                    Ok(vec![ElementaryUnit::Configuration {
                        codec: ElementaryCodec::Aac,
                        extradata: slice_after(&self.raw, LEGACY_AAC_HEADER_BYTES, "audio")?,
                        track_id: None,
                    }])
                }
                AudioTagBody::Legacy(LegacyAudioTagBody::Aac(AacAudioData::Raw(_))) => {
                    Ok(vec![ElementaryUnit::Sample {
                        codec: ElementaryCodec::Aac,
                        payload: slice_after(&self.raw, LEGACY_AAC_HEADER_BYTES, "audio")?,
                        keyframe: true,
                        composition_time_offset: 0,
                        track_id: None,
                    }])
                }
                AudioTagBody::Legacy(_) => Ok(Vec::new()),
                AudioTagBody::Enhanced(body) => enhanced_audio(body),
            },
        }
    }

    /// The first mapped unit, when the tag carries only one.
    pub fn elementary_unit(&self) -> Result<Option<ElementaryUnit>, MediaValidationError> {
        Ok(self.elementary_units()?.into_iter().next())
    }
}

impl ValidatedMedia<ParsedVideo> {
    /// Maps a validated video message onto elementary units.
    ///
    /// A packed Enhanced `ManyTracks` tag yields one unit per mapped track.
    pub fn elementary_units(&self) -> Result<Vec<ElementaryUnit>, MediaValidationError> {
        match &self.interpretation {
            MediaInterpretation::Opaque { reason } => Err(MediaValidationError::Malformed {
                kind: "video",
                reason: reason.clone(),
            }),
            MediaInterpretation::Parsed(parsed) => {
                let keyframe = parsed.header.frame_type == VideoFrameType::KeyFrame
                    || parsed.header.frame_type == VideoFrameType::GeneratedKeyFrame;
                match (&parsed.header.data, &parsed.body) {
                    (
                        VideoTagHeaderData::Legacy(LegacyVideoTagHeader::AvcPacket(
                            LegacyVideoTagHeaderAvcPacket::SequenceHeader,
                        )),
                        _,
                    ) => Ok(vec![ElementaryUnit::Configuration {
                        codec: ElementaryCodec::Avc,
                        extradata: slice_after(&self.raw, LEGACY_AVC_HEADER_BYTES, "video")?,
                        track_id: None,
                    }]),
                    (
                        VideoTagHeaderData::Legacy(LegacyVideoTagHeader::AvcPacket(
                            LegacyVideoTagHeaderAvcPacket::Nalu {
                                composition_time_offset,
                            },
                        )),
                        VideoTagBody::Legacy(LegacyVideoTagBody::Other { .. }),
                    ) => Ok(vec![ElementaryUnit::Sample {
                        codec: ElementaryCodec::Avc,
                        payload: slice_after(&self.raw, LEGACY_AVC_HEADER_BYTES, "video")?,
                        keyframe,
                        composition_time_offset: signed_cts(*composition_time_offset),
                        track_id: None,
                    }]),
                    (_, VideoTagBody::Enhanced(body)) => enhanced_video(body, keyframe),
                    _ => Ok(Vec::new()),
                }
            }
        }
    }

    /// The first mapped unit, when the tag carries only one.
    pub fn elementary_unit(&self) -> Result<Option<ElementaryUnit>, MediaValidationError> {
        Ok(self.elementary_units()?.into_iter().next())
    }
}

const LEGACY_AAC_HEADER_BYTES: usize = 2;
const LEGACY_AVC_HEADER_BYTES: usize = 5;

fn slice_after(
    raw: &Bytes,
    header_bytes: usize,
    kind: &'static str,
) -> Result<Bytes, MediaValidationError> {
    if raw.len() < header_bytes {
        return Err(MediaValidationError::Malformed {
            kind,
            reason: "tag is shorter than its FLV header".into(),
        });
    }
    Ok(raw.slice(header_bytes..))
}

/// FLV stores composition time as signed 24-bit; the demuxer surfaces it as `u32`.
fn signed_cts(value: u32) -> i32 {
    let value = value & 0x00ff_ffff;
    if value & 0x0080_0000 == 0 {
        value as i32
    } else {
        (value | 0xff00_0000) as i32
    }
}

fn enhanced_audio(body: &ExAudioTagBody) -> Result<Vec<ElementaryUnit>, MediaValidationError> {
    match body {
        ExAudioTagBody::NoMultitrack {
            audio_four_cc,
            packet,
        } => Ok(audio_packet(*audio_four_cc, packet, None)?
            .into_iter()
            .collect()),
        ExAudioTagBody::ManyTracks(tracks) => {
            let mut units = Vec::with_capacity(tracks.len());
            for track in tracks {
                if let Some(unit) = audio_packet(
                    track.audio_four_cc,
                    &track.packet,
                    Some(track.audio_track_id),
                )? {
                    units.push(unit);
                }
            }
            Ok(units)
        }
    }
}

fn enhanced_video(
    body: &ExVideoTagBody<'_>,
    keyframe: bool,
) -> Result<Vec<ElementaryUnit>, MediaValidationError> {
    match body {
        ExVideoTagBody::Command => Ok(Vec::new()),
        ExVideoTagBody::NoMultitrack {
            video_four_cc,
            packet,
        } => Ok(video_packet(*video_four_cc, packet, keyframe, None)?
            .into_iter()
            .collect()),
        ExVideoTagBody::ManyTracks(tracks) => {
            let mut units = Vec::with_capacity(tracks.len());
            for track in tracks {
                if let Some(unit) = video_packet(
                    track.video_four_cc,
                    &track.packet,
                    keyframe,
                    Some(track.video_track_id),
                )? {
                    units.push(unit);
                }
            }
            Ok(units)
        }
    }
}

fn audio_packet(
    four_cc: AudioFourCc,
    packet: &AudioPacket,
    track_id: Option<u8>,
) -> Result<Option<ElementaryUnit>, MediaValidationError> {
    let codec = match four_cc {
        AudioFourCc::Aac => ElementaryCodec::Aac,
        AudioFourCc::Opus => ElementaryCodec::Opus,
        _ => return Ok(None),
    };
    match packet {
        AudioPacket::SequenceStart { header_data } => Ok(Some(ElementaryUnit::Configuration {
            codec,
            extradata: header_data.clone(),
            track_id,
        })),
        AudioPacket::CodedFrames { data } => Ok(Some(ElementaryUnit::Sample {
            codec,
            payload: data.clone(),
            keyframe: true,
            composition_time_offset: 0,
            track_id,
        })),
        _ => Ok(None),
    }
}

fn video_packet(
    four_cc: VideoFourCc,
    packet: &VideoPacket<'_>,
    keyframe: bool,
    track_id: Option<u8>,
) -> Result<Option<ElementaryUnit>, MediaValidationError> {
    let codec = match four_cc {
        VideoFourCc::Avc => ElementaryCodec::Avc,
        VideoFourCc::Hevc => ElementaryCodec::Hevc,
        VideoFourCc::Av1 => ElementaryCodec::Av1,
        _ => return Ok(None),
    };
    match packet {
        VideoPacket::SequenceStart(start) => Ok(Some(ElementaryUnit::Configuration {
            codec,
            extradata: sequence_start_bytes(start)?,
            track_id,
        })),
        VideoPacket::CodedFrames(frames) => {
            let (payload, composition_time_offset) = match frames {
                VideoPacketCodedFrames::Avc {
                    composition_time_offset,
                    data,
                }
                | VideoPacketCodedFrames::Hevc {
                    composition_time_offset,
                    data,
                } => (data.clone(), *composition_time_offset),
                VideoPacketCodedFrames::Other(data) => (data.clone(), 0),
            };
            Ok(Some(ElementaryUnit::Sample {
                codec,
                payload,
                keyframe,
                composition_time_offset,
                track_id,
            }))
        }
        VideoPacket::CodedFramesX { data } => Ok(Some(ElementaryUnit::Sample {
            codec,
            payload: data.clone(),
            keyframe,
            composition_time_offset: 0,
            track_id,
        })),
        _ => Ok(None),
    }
}

fn sequence_start_bytes(start: &VideoPacketSequenceStart) -> Result<Bytes, MediaValidationError> {
    match start {
        VideoPacketSequenceStart::Avc(record) => {
            let mut bytes = Vec::new();
            record
                .build(&mut bytes)
                .map_err(|error| MediaValidationError::Malformed {
                    kind: "video",
                    reason: format!("could not serialize avcC: {error}"),
                })?;
            Ok(Bytes::from(bytes))
        }
        VideoPacketSequenceStart::Hevc(record) => {
            let mut bytes = Vec::new();
            record
                .mux(&mut bytes)
                .map_err(|error| MediaValidationError::Malformed {
                    kind: "video",
                    reason: format!("could not serialize hvcC: {error}"),
                })?;
            Ok(Bytes::from(bytes))
        }
        VideoPacketSequenceStart::Av1(record) => {
            let mut bytes = Vec::new();
            record
                .mux(&mut bytes)
                .map_err(|error| MediaValidationError::Malformed {
                    kind: "video",
                    reason: format!("could not serialize av1C: {error}"),
                })?;
            Ok(Bytes::from(bytes))
        }
        VideoPacketSequenceStart::Other(bytes) => Ok(bytes.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnhancedValidationMode;

    #[test]
    fn legacy_aac_sequence_header_is_the_audio_specific_config() {
        let raw = Bytes::from_static(&[0xaf, 0x00, 0x11, 0x88]);
        let media =
            ValidatedMedia::parse_audio(raw, EnhancedValidationMode::Strict).expect("legacy AAC");
        match media.elementary_unit().expect("maps") {
            Some(ElementaryUnit::Configuration {
                codec: ElementaryCodec::Aac,
                extradata,
                track_id: None,
            }) => assert_eq!(extradata.as_ref(), &[0x11, 0x88]),
            other => panic!("expected AAC config, got {other:?}"),
        }
    }

    #[test]
    fn legacy_aac_raw_drops_the_flv_packet_type() {
        let raw = Bytes::from_static(&[0xaf, 0x01, 0xde, 0x02, 0x00]);
        let media =
            ValidatedMedia::parse_audio(raw, EnhancedValidationMode::Strict).expect("legacy AAC");
        match media.elementary_unit().expect("maps") {
            Some(ElementaryUnit::Sample {
                codec: ElementaryCodec::Aac,
                payload,
                keyframe: true,
                composition_time_offset: 0,
                track_id: None,
            }) => assert_eq!(payload.as_ref(), &[0xde, 0x02, 0x00]),
            other => panic!("expected AAC sample, got {other:?}"),
        }
    }

    #[test]
    fn legacy_avc_nalu_is_length_prefixed_and_keeps_signed_cts() {
        let mut raw = vec![0x17, 0x01, 0xff, 0xff, 0xff];
        raw.extend_from_slice(&[0x00, 0x00, 0x00, 0x04, 0x65, 0x88, 0x84, 0x05]);
        let media = ValidatedMedia::parse_video(Bytes::from(raw), EnhancedValidationMode::Strict)
            .expect("legacy AVC");
        match media.elementary_unit().expect("maps") {
            Some(ElementaryUnit::Sample {
                codec: ElementaryCodec::Avc,
                payload,
                keyframe: true,
                composition_time_offset,
                track_id: None,
            }) => {
                assert_eq!(
                    payload.as_ref(),
                    &[0x00, 0x00, 0x00, 0x04, 0x65, 0x88, 0x84, 0x05]
                );
                assert_eq!(composition_time_offset, -1);
            }
            other => panic!("expected AVC sample, got {other:?}"),
        }
    }

    #[test]
    fn mp3_and_commands_are_skipped() {
        // Legacy MP3: sound format 2, 44.1 kHz, 16-bit, stereo.
        let mp3 = Bytes::from_static(&[0x2f, 0xff, 0xfb]);
        let media =
            ValidatedMedia::parse_audio(mp3, EnhancedValidationMode::Strict).expect("legacy MP3");
        assert!(media.elementary_unit().expect("skips").is_none());
    }

    #[test]
    fn enhanced_aac_sequence_start_is_header_data() {
        let mut raw = b"\x90mp4a".to_vec();
        raw.extend_from_slice(&[0x11, 0x88]);
        let media = ValidatedMedia::parse_audio(Bytes::from(raw), EnhancedValidationMode::Strict)
            .expect("enhanced AAC");
        match media.elementary_unit().expect("maps") {
            Some(ElementaryUnit::Configuration {
                codec: ElementaryCodec::Aac,
                extradata,
                ..
            }) => assert_eq!(extradata.as_ref(), &[0x11, 0x88]),
            other => panic!("expected AAC config, got {other:?}"),
        }
    }

    #[test]
    fn one_track_aac_names_the_enhanced_track_id() {
        let mut raw = vec![0x95, 0x00, b'm', b'p', b'4', b'a', 2];
        raw.extend_from_slice(&[0x11, 0x88]);
        let media = ValidatedMedia::parse_audio(Bytes::from(raw), EnhancedValidationMode::Strict)
            .expect("OneTrack AAC");
        match media.elementary_unit().expect("maps") {
            Some(ElementaryUnit::Configuration {
                codec: ElementaryCodec::Aac,
                track_id: Some(2),
                extradata,
            }) => assert_eq!(extradata.as_ref(), &[0x11, 0x88]),
            other => panic!("expected track 2 AAC config, got {other:?}"),
        }
    }

    #[test]
    fn packed_many_tracks_yields_one_unit_per_mapped_track() {
        let mut raw = vec![0x95, 0x10, b'm', b'p', b'4', b'a'];
        for (id, payload) in [(1_u8, [0x11, 0x88]), (3, [0x12, 0x10])] {
            raw.push(id);
            raw.extend_from_slice(&[0x00, 0x00, payload.len() as u8]);
            raw.extend_from_slice(&payload);
        }
        let media = ValidatedMedia::parse_audio(Bytes::from(raw), EnhancedValidationMode::Strict)
            .expect("ManyTracks AAC");
        let units = media.elementary_units().expect("maps");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].track_id(), Some(1));
        assert_eq!(units[1].track_id(), Some(3));
        assert!(
            units
                .iter()
                .all(|unit| unit.codec() == ElementaryCodec::Aac)
        );
    }
}
