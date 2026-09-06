//! Maps transmux MPEG-TS events onto rushls tracks and packets.
//!
//! The demuxer already converts Annex-B video to length-prefixed NALs and ADTS
//! AAC to raw frames plus AudioSpecificConfig. This module only copies those
//! facts into the domain types the rest of the pipeline already understands.

use std::num::{NonZeroU16, NonZeroU32};

use transmux::{CodecConfig, Sample, TrackSpec};

use crate::{
    domain::{
        AudioTiming, Codec, DiscoveredTrack, FrameRate, MediaParameters, Payload, SourceTrackKey,
        Timebase, TrackId,
    },
    source::{DiscoveryProblem, Packet, SourceError},
};

/// Builds a domain track from a resolved elementary stream, or skips it.
///
/// Unknown or policy-irrelevant PIDs (`Data`, AC-3, subtitles, …) return
/// `None` so they never enter the catalog. Admission still decides which of
/// the mapped codecs a stream may publish.
pub fn track(spec: &TrackSpec) -> Result<Option<DiscoveredTrack>, SourceError> {
    let Some((codec, parameters, codec_extradata)) = map_config(&spec.config)? else {
        return Ok(None);
    };
    let timescale = NonZeroU32::new(spec.timescale).ok_or(DiscoveryProblem::NotPositive {
        field: "MPEG-TS media timescale",
    })?;
    Ok(Some(DiscoveredTrack {
        id: TrackId(spec.track_id),
        source_key: spec
            .source_pid
            .map(|pid| SourceTrackKey::new(format!("pid/{pid}"))),
        codec,
        parameters,
        timebase: Timebase::new(nz::u32!(1), timescale),
        first_pts: None,
        title: None,
        language: language_from_es_info(&spec.es_info_descriptors),
        codec_extradata,
    }))
}

pub fn packet(
    track_id: TrackId,
    sample: Sample,
    maximum_payload: usize,
) -> Result<Packet, SourceError> {
    let payload = Payload::from_bytes(sample.data);
    if payload.len() > maximum_payload {
        return Err(SourceError::PacketPayloadTooLarge {
            limit: maximum_payload,
            found: payload.len(),
        });
    }
    Ok(Packet {
        track_id,
        pts: sample.pts,
        dts: sample.dts,
        duration: sample
            .duration
            .filter(|duration| *duration > 0)
            .map(i64::from),
        random_access: sample.flags.is_sync,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload,
    })
}

fn map_config(
    config: &CodecConfig,
) -> Result<Option<(Codec, MediaParameters, Payload)>, SourceError> {
    match config {
        CodecConfig::Avc {
            config,
            width,
            height,
        } => {
            let parameters = video_parameters(*width, *height, avc_frame_rate(&config.config))?;
            let extradata = serialize_record(&config.config, "H.264 decoder configuration")?;
            Ok(Some((Codec::H264, parameters, extradata)))
        }
        CodecConfig::Hevc {
            config,
            width,
            height,
        } => {
            let parameters = video_parameters(*width, *height, hevc_frame_rate(&config.config))?;
            let extradata = serialize_record(&config.config, "H.265 decoder configuration")?;
            Ok(Some((Codec::Hevc, parameters, extradata)))
        }
        CodecConfig::Av1 {
            config,
            width,
            height,
        } => {
            let parameters = video_parameters(*width, *height, None)?;
            let extradata = serialize_record(config, "AV1 decoder configuration")?;
            Ok(Some((Codec::Av1, parameters, extradata)))
        }
        CodecConfig::Aac {
            esds,
            channel_count,
            sample_rate,
            sample_size,
        } => {
            let extradata = aac_extradata(esds)?;
            Ok(Some((
                Codec::Aac,
                audio_parameters(*sample_rate, *channel_count, *sample_size)?,
                extradata,
            )))
        }
        CodecConfig::Opus {
            config,
            channel_count,
            sample_rate,
            sample_size,
        } => Ok(Some((
            Codec::Opus,
            audio_parameters(*sample_rate, *channel_count, *sample_size)?,
            serialize_record(config, "Opus decoder configuration")?,
        ))),
        _ => Ok(None),
    }
}

fn video_parameters(
    width: u16,
    height: u16,
    frame_rate: Option<FrameRate>,
) -> Result<MediaParameters, SourceError> {
    Ok(MediaParameters::Video {
        width: nonzero_u32(u32::from(width), "video width")?,
        height: nonzero_u32(u32::from(height), "video height")?,
        frame_rate,
        video_delay: 0,
    })
}

/// H.264 VUI timing is `time_scale / (2 × num_units_in_tick)` (ITU-T H.264 §E.2.1).
///
/// The last access unit of a stream has no successor DTS, and a one-frame
/// PES gets duration 0 from the demuxer. The declared cadence is what lets
/// the pass-through normalizer time that tail.
fn avc_frame_rate(record: &transmux::AVCDecoderConfigurationRecord) -> Option<FrameRate> {
    let info = record.sps.first()?.decode().ok()?;
    let units = NonZeroU32::new(info.num_units_in_tick?)?;
    let scale = NonZeroU32::new(info.time_scale?)?;
    Some(FrameRate::new(scale, units.checked_mul(nz::u32!(2))?))
}

/// HEVC VUI timing is `time_scale / num_units_in_tick` (ITU-T H.265 §E.2.1).
fn hevc_frame_rate(record: &transmux::HEVCDecoderConfigurationRecord) -> Option<FrameRate> {
    let info = record
        .arrays
        .iter()
        .flat_map(|array| array.nalus.iter())
        .find_map(|nal| nal.decode_sps().ok().flatten())?;
    let units = NonZeroU32::new(info.num_units_in_tick?)?;
    let scale = NonZeroU32::new(info.time_scale?)?;
    Some(FrameRate::new(scale, units))
}

fn audio_parameters(
    sample_rate: u32,
    channel_count: u16,
    sample_size: u16,
) -> Result<MediaParameters, SourceError> {
    Ok(MediaParameters::Audio {
        sample_rate: nonzero_u32(sample_rate, "audio sample rate")?,
        channels: NonZeroU16::new(channel_count).ok_or(DiscoveryProblem::NotPositive {
            field: "audio channels",
        })?,
        frame_size: None,
        bit_depth: NonZeroU16::new(sample_size),
        timing: AudioTiming::default(),
    })
}

fn aac_extradata(esds: &transmux::EsdsBox) -> Result<Payload, SourceError> {
    let data = esds
        .es_descriptor
        .decoder_config
        .as_ref()
        .and_then(|config| config.decoder_specific_info.as_ref())
        .map(|info| info.data.clone())
        .filter(|data| !data.is_empty())
        .ok_or(DiscoveryProblem::Missing {
            field: "MPEG-TS AAC codec configuration",
        })?;
    Ok(Payload::from(data))
}

fn serialize_record<T>(value: &T, field: &'static str) -> Result<Payload, SourceError>
where
    T: broadcast_common::Serialize,
    T::Error: core::fmt::Debug,
{
    let bytes = value.to_bytes();
    if bytes.is_empty() {
        return Err(DiscoveryProblem::Missing { field }.into());
    }
    Ok(Payload::from(bytes))
}

fn nonzero_u32(value: u32, field: &'static str) -> Result<NonZeroU32, SourceError> {
    NonZeroU32::new(value).ok_or_else(|| DiscoveryProblem::NotPositive { field }.into())
}

/// First `ISO_639_language_descriptor` (tag `0x0A`) on a PMT ES_info loop.
///
/// ETSI EN 300 468 §6.2.19 repeats `{ ISO_639_language_code, audio_type }`.
/// HLS `LANGUAGE` is a single tag, so the first three-letter code is the
/// advertised language. Unknown or truncated descriptors are skipped: a
/// malformed SI loop must not fail discovery of an otherwise usable PID.
fn language_from_es_info(descriptors: &[u8]) -> Option<String> {
    const ISO_639_LANGUAGE_DESCRIPTOR_TAG: u8 = 0x0A;
    let mut offset = 0;
    while offset + 2 <= descriptors.len() {
        let tag = descriptors[offset];
        let length = usize::from(descriptors[offset + 1]);
        let body_start = offset + 2;
        let Some(body_end) = body_start.checked_add(length) else {
            break;
        };
        if body_end > descriptors.len() {
            break;
        }
        if tag == ISO_639_LANGUAGE_DESCRIPTOR_TAG && length >= 4 {
            let code = &descriptors[body_start..body_start + 3];
            if code.iter().all(u8::is_ascii_alphabetic)
                && let Ok(text) = std::str::from_utf8(code)
            {
                return Some(text.to_ascii_lowercase());
            }
        }
        offset = body_end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::language_from_es_info;

    #[test]
    fn reads_the_first_iso_639_code_from_the_es_info_loop() {
        // Unknown tag, then ISO_639_language_descriptor { "eng", audio_type 0 }.
        let descriptors = [0x05, 0x01, 0xff, 0x0a, 0x04, b'e', b'n', b'g', 0x00];
        assert_eq!(language_from_es_info(&descriptors).as_deref(), Some("eng"));
    }

    #[test]
    fn takes_the_first_code_when_the_descriptor_repeats() {
        let descriptors = [0x0a, 0x08, b's', b'p', b'a', 0x00, b'e', b'n', b'g', 0x00];
        assert_eq!(language_from_es_info(&descriptors).as_deref(), Some("spa"));
    }

    #[test]
    fn ignores_empty_truncated_and_non_alphabetic_descriptors() {
        assert_eq!(language_from_es_info(&[]), None);
        assert_eq!(language_from_es_info(&[0x0a, 0x04, b'e', b'n']), None);
        assert_eq!(
            language_from_es_info(&[0x0a, 0x04, b'e', b'n', b'1', 0x00]),
            None
        );
    }
}
