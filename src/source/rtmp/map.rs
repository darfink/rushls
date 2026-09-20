//! Maps elementary RTMP units onto rushls tracks and packets.
//!
//! Opus timestamps use 48 kHz to preserve exact pre-skip. Other tracks keep
//! the RTMP millisecond clock. Decoder configuration is
//! parsed only far enough to fill [`DiscoveredTrack`] parameters; the payload
//! bytes themselves are already length-prefixed video or raw AAC/Opus.

use crate::media::video_config::{h264_frame_rate, hevc_frame_rate};
use std::num::NonZeroU32;

use broadcast_common::Parse;
use bytes::Bytes;
use rtmpx::{ElementaryCodec, ElementaryUnit, EncoderSummary};

use crate::{
    domain::{
        Codec, DiscoveredTrack, FrameRate, MediaParameters, Payload, SourceTrackKey, Timebase,
        TrackId,
    },
    source::{DiscoveryProblem, Packet, SourceError},
};

pub const TIMEBASE: Timebase = Timebase::new(nz::u32!(1), nz::u32!(1_000));

pub fn track(
    id: TrackId,
    codec: ElementaryCodec,
    extradata: Bytes,
    track_id: Option<u8>,
    hint: Option<&EncoderSummary>,
) -> Result<DiscoveredTrack, SourceError> {
    let (mapped, mut parameters) = match codec {
        ElementaryCodec::Avc => (Codec::H264, video_parameters(&extradata, hint, "H.264")?),
        ElementaryCodec::Hevc => (Codec::Hevc, video_parameters_hevc(&extradata, hint)?),
        ElementaryCodec::Av1 => (Codec::Av1, video_parameters_from_hint(hint, "AV1")?),
        ElementaryCodec::Aac => (
            Codec::Aac,
            crate::media::aac::parameters(&extradata).map_err(SourceError::Demux)?,
        ),
        ElementaryCodec::Opus => (
            Codec::Opus,
            crate::media::opus::parameters(&extradata).map_err(SourceError::Demux)?,
        ),
        _ => {
            return Err(SourceError::Demux("unsupported elementary codec".into()));
        }
    };
    if let MediaParameters::Video {
        video_delay,
        frame_rate,
        ..
    } = &mut parameters
    {
        *frame_rate = frame_rate.or_else(|| {
            hint.and_then(|h| h.framerate)
                .and_then(crate::media::cadence::nominal_rate)
        });
        *video_delay = crate::media::video_config::properties(mapped, &extradata).reorder_depth;
    }
    Ok(DiscoveredTrack {
        decoder_config_origin: crate::domain::DecoderConfigOrigin::Publisher,
        video_cadence: crate::domain::VideoCadence::Unknown,
        id,
        source_key: Some(source_key(codec, track_id)),
        codec: mapped,
        parameters,
        timebase: if codec == ElementaryCodec::Opus {
            Timebase::new(nz::u32!(1), nz::u32!(48_000))
        } else {
            TIMEBASE
        },
        first_pts: None,
        title: None,
        language: None,
        codec_extradata: Payload::from_bytes(extradata),
    })
}

pub fn packet(
    track_id: TrackId,
    timestamp: u32,
    unit: ElementaryUnit,
    maximum_payload: usize,
) -> Result<Packet, SourceError> {
    let ElementaryUnit::Sample {
        payload,
        keyframe,
        composition_time_offset,
        codec,
        ..
    } = unit
    else {
        return Err(SourceError::Demux(
            "RTMP configuration cannot be emitted as a packet".into(),
        ));
    };
    if payload.len() > maximum_payload {
        return Err(SourceError::PacketPayloadTooLarge {
            limit: maximum_payload,
            found: payload.len(),
        });
    }
    let dts = i64::from(timestamp)
        * if codec == ElementaryCodec::Opus {
            48
        } else {
            1
        };
    let pts = dts.saturating_add(i64::from(composition_time_offset));
    Ok(Packet {
        track_id,
        pts: Some(pts),
        dts: Some(dts),
        duration: if codec == ElementaryCodec::Opus {
            Some(i64::from(
                crate::media::opus::packet_samples(&payload).map_err(SourceError::Demux)?,
            ))
        } else {
            None
        },
        random_access: keyframe,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: Payload::from_bytes(payload),
    })
}

/// Script-data captions are UTF-8 on the RTMP millisecond clock, with no
/// duration: the cue shows until the next one replaces it.
pub fn text_track(id: TrackId) -> DiscoveredTrack {
    DiscoveredTrack {
        decoder_config_origin: crate::domain::DecoderConfigOrigin::Publisher,
        video_cadence: crate::domain::VideoCadence::Unknown,
        id,
        source_key: Some(SourceTrackKey::new("script/text")),
        codec: Codec::Text,
        parameters: MediaParameters::Subtitle,
        timebase: TIMEBASE,
        first_pts: None,
        title: None,
        language: None,
        codec_extradata: Payload::from(Vec::new()),
    }
}

pub fn text_packet(
    track_id: TrackId,
    timestamp: u32,
    text: String,
    maximum_payload: usize,
) -> Result<Packet, SourceError> {
    if text.len() > maximum_payload {
        return Err(SourceError::PacketPayloadTooLarge {
            limit: maximum_payload,
            found: text.len(),
        });
    }
    let pts = i64::from(timestamp);
    Ok(Packet {
        track_id,
        pts: Some(pts),
        dts: Some(pts),
        duration: None,
        random_access: true,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: Payload::from(text.into_bytes()),
    })
}

/// Stable identity for one RTMP elementary track.
///
/// Legacy and Enhanced `NoMultitrack` share `audio` / `video`. Numbered
/// Enhanced tracks keep their id so two AAC languages do not collide.
pub(crate) fn source_key(codec: ElementaryCodec, track_id: Option<u8>) -> SourceTrackKey {
    let kind = if codec.is_video() { "video" } else { "audio" };
    SourceTrackKey::new(match track_id {
        Some(id) => format!("{kind}/{id}"),
        None => kind.to_owned(),
    })
}

fn video_parameters(
    extradata: &[u8],
    hint: Option<&EncoderSummary>,
    codec: &'static str,
) -> Result<MediaParameters, SourceError> {
    if let Ok(record) = transmux::AVCDecoderConfigurationRecord::parse(extradata)
        && let Some(sps) = record.sps.first()
        && let Ok(info) = sps.decode()
    {
        return video_size(
            info.width,
            info.height,
            h264_frame_rate(info.num_units_in_tick, info.time_scale),
        );
    }
    video_parameters_from_hint(hint, codec)
}

fn video_parameters_hevc(
    extradata: &[u8],
    hint: Option<&EncoderSummary>,
) -> Result<MediaParameters, SourceError> {
    if let Ok(record) = transmux::HEVCDecoderConfigurationRecord::parse(extradata) {
        for array in &record.arrays {
            for nalu in &array.nalus {
                if let Ok(Some(info)) = nalu.decode_sps() {
                    return video_size(
                        info.width,
                        info.height,
                        hevc_frame_rate(info.num_units_in_tick, info.time_scale),
                    );
                }
            }
        }
    }
    video_parameters_from_hint(hint, "H.265")
}

fn video_parameters_from_hint(
    hint: Option<&EncoderSummary>,
    codec: &'static str,
) -> Result<MediaParameters, SourceError> {
    let (width, height) = hint
        .and_then(|hint| Some((hint.width?, hint.height?)))
        .ok_or(DiscoveryProblem::Missing {
            field: match codec {
                "H.264" => "H.264 video dimensions",
                "H.265" => "H.265 video dimensions",
                _ => "video dimensions",
            },
        })?;
    video_size(width, height, None)
}

fn video_size(
    width: u32,
    height: u32,
    frame_rate: Option<FrameRate>,
) -> Result<MediaParameters, SourceError> {
    Ok(MediaParameters::Video {
        width: nonzero_u32(width, "video width")?,
        height: nonzero_u32(height, "video height")?,
        frame_rate,
        video_delay: 0,
    })
}

fn nonzero_u32(value: u32, field: &'static str) -> Result<NonZeroU32, SourceError> {
    NonZeroU32::new(value).ok_or_else(|| DiscoveryProblem::NotPositive { field }.into())
}

pub fn codec(codec: ElementaryCodec) -> Codec {
    match codec {
        ElementaryCodec::Avc => Codec::H264,
        ElementaryCodec::Hevc => Codec::Hevc,
        ElementaryCodec::Av1 => Codec::Av1,
        ElementaryCodec::Aac => Codec::Aac,
        ElementaryCodec::Opus => Codec::Opus,
        // `ElementaryCodec` is non-exhaustive: a future rtmpx codec has no
        // numeric identity here, so it compares unequal to every discovered
        // codec and the live path rejects it as changed parameters.
        _ => Codec::Unknown(u32::MAX),
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    #[test]
    fn legacy_and_enhanced_rtmp_preserve_codec_cadence() -> Result<(), SourceError> {
        use crate::media::fixtures::{AV1_FIXED_CADENCE, H264_FIXED_CADENCE, HEVC_FIXED_CADENCE};
        let mut hint = EncoderSummary::default();
        hint.width = Some(64);
        hint.height = Some(64);
        hint.framerate = Some(30.0);
        for (codec, bytes) in [
            (ElementaryCodec::Avc, H264_FIXED_CADENCE),
            (ElementaryCodec::Hevc, HEVC_FIXED_CADENCE),
            (ElementaryCodec::Av1, AV1_FIXED_CADENCE),
        ] {
            let track = track(
                TrackId(0),
                codec,
                Bytes::from_static(bytes),
                None,
                Some(&hint),
            )?;
            let cadence = crate::media::cadence::inspect(&track);
            assert!(matches!(cadence, crate::domain::VideoCadence::Fixed { .. }));
            assert_eq!(
                cadence.rate(),
                Some(FrameRate::new(nz::u32!(25), nz::u32!(1)))
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod gap_tests {
    use super::*;
    use crate::media::{NormalizedMedia, NormalizerFactory};

    #[test]
    fn rtmp_packet_loss_reaches_exact_video_gap_detection() -> Result<(), Box<dyn std::error::Error>>
    {
        let units = crate::media::fixtures::H264_CFR_FLV_UNITS;
        let config = rtmpx::ValidatedMedia::parse_video(
            Bytes::from_static(units[0].1),
            rtmpx::EnhancedValidationMode::Strict,
        )?;
        let Some(ElementaryUnit::Configuration {
            codec,
            extradata,
            track_id,
            ..
        }) = config.elementary_unit()?
        else {
            panic!("sequence header")
        };
        let mut track = track(TrackId(0), codec, extradata, track_id, None)?;
        track.first_pts = Some(0);
        let presentation = crate::media::fixtures::presentation(vec![track]);
        let timeline = crate::media::calibrate(&presentation)?;
        for mode in [
            crate::domain::InputMode::Strict,
            crate::domain::InputMode::Permissive,
        ] {
            let mut started =
                crate::media::PassThroughNormalizerFactory.start(&presentation, &timeline, mode)?;
            assert!(matches!(
                started.presentation.tracks()[0].video_cadence,
                crate::domain::VideoCadence::Fixed { .. }
            ));
            let mut out = Vec::new();
            for &(timestamp, payload) in &units[1..] {
                if timestamp == 40 {
                    continue;
                }
                let media = rtmpx::ValidatedMedia::parse_video(
                    Bytes::from_static(payload),
                    rtmpx::EnhancedValidationMode::Strict,
                )?;
                let packet = packet(
                    TrackId(0),
                    timestamp,
                    media.elementary_unit()?.expect("coded picture"),
                    1024,
                )?;
                let result = started.normalizer.push(packet, &mut out);
                if mode == crate::domain::InputMode::Strict && timestamp == 80 {
                    assert!(matches!(
                        result,
                        Err(crate::media::NormalizeError::Timestamp(_))
                    ));
                    assert!(out.is_empty());
                    break;
                }
                result?;
            }
            if mode == crate::domain::InputMode::Permissive {
                started.normalizer.finish(&mut out)?;
                assert_eq!(out.len(), 4);
                assert!(
                    matches!(&out[1], NormalizedMedia::Gap(g) if g.start == 3600 && g.end == 7200)
                );
                assert!(
                    matches!(&out[2], NormalizedMedia::Video(v) if v.pts == 7200 && v.dts == 7200 && v.duration == 3600 && v.payload.as_bytes() == &units[3].1[5..])
                );
                assert_eq!(started.normalizer.take_notices().len(), 1);
            }
        }
        Ok(())
    }
}
