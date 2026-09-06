//! Opus in TS private PES, carried as opaque Data by transmux 0.24.
//! TS control headers carry packet lengths and trim counts; PMT descriptors
//! carry the channel configuration. Neither is an OpusHead packet.

use crate::{
    domain::{AudioTiming, Codec, DiscoveredTrack, MediaParameters, Payload, Timebase, TrackId},
    source::{Packet, SourceError},
};
use broadcast_common::Serialize;
use transmux::{CodecConfig, TrackSpec};

pub fn track(spec: &TrackSpec) -> Result<Option<DiscoveredTrack>, SourceError> {
    let CodecConfig::Data {
        stream_type: 6,
        descriptors,
        ..
    } = &spec.config
    else {
        return Ok(None);
    };
    let mut data = &descriptors[..];
    let mut registered = false;
    let mut channels = None;
    while data.len() >= 2 {
        let length = usize::from(data[1]);
        let Some(body) = data.get(2..2 + length) else {
            return Err(invalid("truncated Opus PMT descriptor"));
        };
        if data[0] == 5 && body.starts_with(b"Opus") {
            registered = true;
        }
        if data[0] == 0x7f && body.first() == Some(&0x80) {
            channels = body.get(1).copied();
        }
        data = &data[2 + length..];
    }
    if !registered {
        return Ok(None);
    }
    // Extended mappings need explicit stream/coupled counts. Refuse them
    // rather than creating a playable-looking but incorrect stereo track.
    let channels = channels
        .filter(|count| matches!(count, 1 | 2))
        .ok_or_else(|| invalid("TS Opus currently requires a mono or stereo channel descriptor"))?;
    let config = transmux::OpusSpecificBox {
        version: 0,
        output_channel_count: channels,
        pre_skip: 0,
        input_sample_rate: 48_000,
        output_gain: 0,
        channel_mapping_family: 0,
        channel_mapping: None,
    };
    Ok(Some(DiscoveredTrack {
        id: TrackId(spec.track_id),
        source_key: spec
            .source_pid
            .map(|pid| crate::domain::SourceTrackKey::new(format!("pid/{pid}"))),
        codec: Codec::Opus,
        parameters: MediaParameters::Audio {
            sample_rate: nz::u32!(48_000),
            channels: std::num::NonZeroU16::new(u16::from(channels)).expect("validated channels"),
            frame_size: None,
            bit_depth: None,
            timing: AudioTiming {
                seek_preroll_samples: 3_840,
                ..AudioTiming::default()
            },
        },
        timebase: Timebase::new(nz::u32!(1), nz::u32!(48_000)),
        first_pts: None,
        title: None,
        language: super::map::language_from_es_info(descriptors),
        codec_extradata: Payload::from(config.to_bytes()),
    }))
}

pub fn packets(
    track: &mut DiscoveredTrack,
    sample: &transmux::Sample,
    maximum_payload: usize,
) -> Result<Vec<Packet>, SourceError> {
    // Opaque TS PES timestamps use the 90 kHz clock. The packet-derived
    // durations and trim counts use 48 kHz and must remain exact.
    let mut pts = sample
        .pts
        .or(sample.dts)
        .ok_or_else(|| invalid("TS Opus PES has no timestamp"))?;
    pts = i64::try_from(i128::from(pts) * 8 / 15)
        .map_err(|_| invalid("TS Opus timestamp overflow"))?;
    let mut offset = 0;
    let data = &sample.data;
    let mut packets = Vec::new();
    while offset < data.len() {
        let header = data
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("truncated TS Opus control header"))?;
        if header[0] != 0x7f || header[1] & 0xe0 != 0xe0 {
            return Err(invalid("invalid TS Opus control header"));
        }
        let flags = header[1];
        offset += 2;
        let mut length = 0usize;
        loop {
            let byte = *data
                .get(offset)
                .ok_or_else(|| invalid("truncated TS Opus packet length"))?;
            offset += 1;
            length = length
                .checked_add(usize::from(byte))
                .ok_or_else(|| invalid("TS Opus length overflow"))?;
            if byte != 255 {
                break;
            }
        }
        let mut trim = crate::domain::AudioTrim::default();
        for (flag, count) in [
            (0x10, &mut trim.leading_samples),
            (8, &mut trim.trailing_samples),
        ] {
            if flags & flag != 0 {
                let bytes = data
                    .get(offset..offset + 2)
                    .ok_or_else(|| invalid("truncated TS Opus trim"))?;
                *count = u32::from(u16::from_be_bytes([bytes[0], bytes[1]]));
                offset += 2;
            }
        }
        if flags & 4 != 0 {
            let size = usize::from(
                *data
                    .get(offset)
                    .ok_or_else(|| invalid("truncated TS Opus extension"))?,
            );
            offset = offset
                .checked_add(size + 1)
                .ok_or_else(|| invalid("TS Opus extension overflow"))?;
        }
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| invalid("truncated TS Opus payload"))?;
        if length > maximum_payload {
            return Err(SourceError::PacketPayloadTooLarge {
                limit: maximum_payload,
                found: length,
            });
        }
        let payload = data.slice(offset..end);
        offset = end;
        let duration = crate::media::opus::packet_samples(&payload).map_err(SourceError::Demux)?;
        if trim.leading_samples + trim.trailing_samples > duration {
            return Err(invalid("TS Opus trim exceeds duration"));
        }
        if track.first_pts.is_none()
            && let MediaParameters::Audio { timing, .. } = &mut track.parameters
        {
            timing.initial_padding_samples = trim.leading_samples;
        }
        let packet = Packet {
            track_id: track.id,
            pts: Some(pts),
            dts: Some(pts),
            duration: Some(i64::from(duration)),
            random_access: true,
            audio_trim: trim,
            webvtt: crate::domain::WebVttCueMetadata::default(),
            subtitle_position: None,
            payload: Payload::from_bytes(payload),
        };
        crate::source::record_first_pts(track, &packet)?;
        packets.push(packet);
        pts = pts
            .checked_add(i64::from(duration))
            .ok_or_else(|| invalid("TS Opus PTS overflow"))?;
    }
    Ok(packets)
}
fn invalid(message: &'static str) -> SourceError {
    SourceError::Demux(message.into())
}
