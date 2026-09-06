//! Maps CMAF-ready RTMP units onto rushls tracks and packets.
//!
//! Timestamps stay on the RTMP millisecond clock. Decoder configuration is
//! parsed only far enough to fill [`DiscoveredTrack`] parameters; the payload
//! bytes themselves are already length-prefixed video or raw AAC/Opus.

use std::num::{NonZeroU16, NonZeroU32};

use broadcast_common::Parse;
use bytes::Bytes;
use cc_rtmp::{CmafCodec, CmafUnit, EncoderSummary};

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
    codec: CmafCodec,
    extradata: Bytes,
    track_id: Option<u8>,
    hint: Option<&EncoderSummary>,
) -> Result<DiscoveredTrack, SourceError> {
    let (mapped, parameters) = match codec {
        CmafCodec::Avc => (Codec::H264, video_parameters(&extradata, hint, "H.264")?),
        CmafCodec::Hevc => (Codec::Hevc, video_parameters_hevc(&extradata, hint)?),
        CmafCodec::Av1 => (Codec::Av1, video_parameters_from_hint(hint, "AV1")?),
        CmafCodec::Aac => (Codec::Aac, aac_parameters(&extradata)?),
        CmafCodec::Opus => (Codec::Opus, opus_parameters(&extradata)?),
    };
    Ok(DiscoveredTrack {
        id,
        source_key: Some(source_key(codec, track_id)),
        codec: mapped,
        parameters,
        timebase: TIMEBASE,
        first_pts: None,
        title: None,
        language: None,
        codec_extradata: Payload::from_bytes(extradata),
    })
}

pub fn packet(
    track_id: TrackId,
    timestamp: u32,
    unit: CmafUnit,
    maximum_payload: usize,
) -> Result<Packet, SourceError> {
    let CmafUnit::Sample {
        payload,
        keyframe,
        composition_time_offset,
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
    let dts = i64::from(timestamp);
    let pts = dts.saturating_add(i64::from(composition_time_offset));
    Ok(Packet {
        track_id,
        pts: Some(pts),
        dts: Some(dts),
        duration: None,
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
pub(crate) fn source_key(codec: CmafCodec, track_id: Option<u8>) -> SourceTrackKey {
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
    let asc =
        transmux::AudioSpecificConfig::parse(extradata).map_err(|_| DiscoveryProblem::Missing {
            field: "RTMP AAC codec configuration",
        })?;
    let sample_rate = aac_sample_rate(&asc).ok_or(DiscoveryProblem::NotPositive {
        field: "audio sample rate",
    })?;
    let channels =
        aac_channels(asc.channel_configuration).ok_or(DiscoveryProblem::NotPositive {
            field: "audio channels",
        })?;
    Ok(MediaParameters::Audio {
        sample_rate: nonzero_u32(sample_rate, "audio sample rate")?,
        channels,
        // RTMP tags have no duration. AAC-LC access units are 1024 decoded
        // samples, which the pass-through normalizer projects onto 1/sample_rate.
        frame_size: Some(nz::u32!(1_024)),
        bit_depth: None,
        timing: AudioTiming::default(),
    })
}

fn opus_parameters(extradata: &[u8]) -> Result<MediaParameters, SourceError> {
    let (sample_rate, channels) = parse_opus_head(extradata).unwrap_or((48_000, 2));
    Ok(MediaParameters::Audio {
        sample_rate: nonzero_u32(sample_rate, "audio sample rate")?,
        channels: NonZeroU16::new(channels).ok_or(DiscoveryProblem::NotPositive {
            field: "audio channels",
        })?,
        frame_size: None,
        bit_depth: None,
        timing: AudioTiming::default(),
    })
}

fn parse_opus_head(bytes: &[u8]) -> Option<(u32, u16)> {
    let body = if bytes.starts_with(b"OpusHead") {
        bytes.get(8..)?
    } else {
        bytes
    };
    let channels = u16::from(*body.first()?);
    let sample_rate = if body.len() >= 8 {
        u32::from_le_bytes(body[4..8].try_into().ok()?)
    } else {
        48_000
    };
    (sample_rate > 0 && channels > 0).then_some((sample_rate, channels))
}

fn aac_sample_rate(asc: &transmux::AudioSpecificConfig) -> Option<u32> {
    if let Some(hz) = asc.sampling_frequency {
        return Some(hz);
    }
    Some(match asc.sampling_frequency_index {
        transmux::SamplingFrequencyIndex::Fs96000 => 96_000,
        transmux::SamplingFrequencyIndex::Fs88200 => 88_200,
        transmux::SamplingFrequencyIndex::Fs64000 => 64_000,
        transmux::SamplingFrequencyIndex::Fs48000 => 48_000,
        transmux::SamplingFrequencyIndex::Fs44100 => 44_100,
        transmux::SamplingFrequencyIndex::Fs32000 => 32_000,
        transmux::SamplingFrequencyIndex::Fs24000 => 24_000,
        transmux::SamplingFrequencyIndex::Fs22050 => 22_050,
        transmux::SamplingFrequencyIndex::Fs16000 => 16_000,
        transmux::SamplingFrequencyIndex::Fs12000 => 12_000,
        transmux::SamplingFrequencyIndex::Fs11025 => 11_025,
        transmux::SamplingFrequencyIndex::Fs8000 => 8_000,
        transmux::SamplingFrequencyIndex::Fs7350 => 7_350,
        transmux::SamplingFrequencyIndex::Escape
        | transmux::SamplingFrequencyIndex::Reserved(_) => {
            return None;
        }
        _ => return None,
    })
}

fn aac_channels(config: transmux::ChannelConfiguration) -> Option<NonZeroU16> {
    let count = match config {
        transmux::ChannelConfiguration::Mono => 1,
        transmux::ChannelConfiguration::Stereo => 2,
        transmux::ChannelConfiguration::Ch3 => 3,
        transmux::ChannelConfiguration::Ch4 => 4,
        transmux::ChannelConfiguration::Ch5 => 5,
        transmux::ChannelConfiguration::Ch5_1 => 6,
        transmux::ChannelConfiguration::Ch7_1 => 8,
        transmux::ChannelConfiguration::InBand | transmux::ChannelConfiguration::Reserved(_) => {
            return None;
        }
        _ => return None,
    };
    NonZeroU16::new(count)
}

fn nonzero_u32(value: u32, field: &'static str) -> Result<NonZeroU32, SourceError> {
    NonZeroU32::new(value).ok_or_else(|| DiscoveryProblem::NotPositive { field }.into())
}
