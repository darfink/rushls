//! Maps elementary RTMP units onto rushls tracks and packets.
//!
//! Opus timestamps use 48 kHz to preserve exact pre-skip. Other tracks keep
//! the RTMP millisecond clock. Decoder configuration is
//! parsed only far enough to fill [`DiscoveredTrack`] parameters; the payload
//! bytes themselves are already length-prefixed video or raw AAC/Opus.

use std::num::{NonZeroU16, NonZeroU32};

use broadcast_common::Parse;
use bytes::Bytes;
use cc_rtmp::{ElementaryCodec, ElementaryUnit, EncoderSummary};

use crate::{
    domain::{
        AudioTiming, Codec, DiscoveredTrack, FrameRate, MediaParameters, Payload, SourceTrackKey,
        Timebase, TrackId,
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
        ElementaryCodec::Aac => (Codec::Aac, aac_parameters(&extradata)?),
        ElementaryCodec::Opus => (Codec::Opus, opus_parameters(&extradata)?),
    };
    if let MediaParameters::Video { video_delay, .. } = &mut parameters {
        *video_delay = crate::media::video_config::properties(mapped, &extradata).reorder_depth;
    }
    Ok(DiscoveredTrack {
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

/// H.264 VUI timing is `time_scale / (2 × num_units_in_tick)` (ITU-T H.264 §E.2.1).
fn h264_frame_rate(num_units_in_tick: Option<u32>, time_scale: Option<u32>) -> Option<FrameRate> {
    let units = NonZeroU32::new(num_units_in_tick?)?;
    let scale = NonZeroU32::new(time_scale?)?;
    Some(FrameRate::new(scale, units.checked_mul(nz::u32!(2))?))
}

/// HEVC VUI timing is `time_scale / num_units_in_tick` (ITU-T H.265 §E.2.1).
fn hevc_frame_rate(num_units_in_tick: Option<u32>, time_scale: Option<u32>) -> Option<FrameRate> {
    let units = NonZeroU32::new(num_units_in_tick?)?;
    let scale = NonZeroU32::new(time_scale?)?;
    Some(FrameRate::new(scale, units))
}

fn aac_parameters(extradata: &[u8]) -> Result<MediaParameters, SourceError> {
    crate::media::aac::parameters(extradata).map_err(SourceError::Demux)
}

fn opus_parameters(extradata: &[u8]) -> Result<MediaParameters, SourceError> {
    let config = crate::media::opus::configuration(extradata).map_err(SourceError::Demux)?;
    Ok(MediaParameters::Audio {
        sample_rate: nz::u32!(48_000),
        channels: NonZeroU16::new(u16::from(config.output_channel_count)).ok_or(
            DiscoveryProblem::NotPositive {
                field: "audio channels",
            },
        )?,
        frame_size: None,
        bit_depth: None,
        timing: AudioTiming {
            initial_padding_samples: u32::from(config.pre_skip),
            seek_preroll_samples: 3_840,
            ..AudioTiming::default()
        },
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
    }
}
