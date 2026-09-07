//! Opus decoder configuration shared by ingest and packaging.

use broadcast_common::Parse;
use transmux::OpusSpecificBox;

/// Converts Ogg/RTMP OpusHead to dOps, preserving gain and channel mapping.
/// dOps uses big endian fields and version zero; OpusHead uses little endian.
pub fn configuration(extra: &[u8]) -> Result<OpusSpecificBox, Box<str>> {
    let converted;
    let body = if let Some(head) = extra.strip_prefix(b"OpusHead") {
        if head.len() < 11 || head[0] == 0 || head[0] > 15 {
            return Err("invalid or truncated OpusHead".into());
        }
        let mut bytes = head.to_vec();
        bytes[0] = 0;
        bytes[2..4].reverse();
        bytes[4..8].reverse();
        bytes[8..10].reverse();
        converted = bytes;
        &converted[..]
    } else if extra.len() >= 8 && &extra[4..8] == b"dOps" {
        &extra[8..]
    } else {
        extra
    };
    let config =
        OpusSpecificBox::parse(body).map_err(|error| error.to_string().into_boxed_str())?;
    if config.version != 0 || config.output_channel_count == 0 {
        return Err("invalid dOps version or channel count".into());
    }
    if config.channel_mapping_family == 0 {
        if config.output_channel_count > 2 {
            return Err("Opus mapping family zero supports only mono or stereo".into());
        }
    } else {
        let table = config
            .channel_mapping
            .as_ref()
            .ok_or("missing Opus channel mapping")?;
        let coded_channels = u16::from(table.stream_count) + u16::from(table.coupled_count);
        if table.channel_mapping.len() != usize::from(config.output_channel_count)
            || table.stream_count == 0
            || table.coupled_count > table.stream_count
            || coded_channels > 255
            || table
                .channel_mapping
                .iter()
                .any(|&channel| channel != 255 && u16::from(channel) >= coded_channels)
        {
            return Err("invalid or truncated Opus channel mapping".into());
        }
    }
    Ok(config)
}

/// Track timing shared by RTMP and MOQ. Opus always decodes at 48 kHz.
pub fn parameters(extradata: &[u8]) -> Result<crate::domain::MediaParameters, Box<str>> {
    use crate::domain::{AudioTiming, MediaParameters};
    let config = configuration(extradata)?;
    Ok(MediaParameters::Audio {
        sample_rate: nz::u32!(48_000),
        channels: std::num::NonZeroU16::new(u16::from(config.output_channel_count))
            .ok_or("zero Opus channels")?,
        frame_size: None,
        bit_depth: None,
        timing: AudioTiming {
            initial_padding_samples: u32::from(config.pre_skip),
            seek_preroll_samples: 3_840,
            ..AudioTiming::default()
        },
    })
}

/// Decoded samples at 48 kHz from RFC 6716's TOC and frame-count byte.
/// Durations must come from the packet: Opus can change duration every packet.
pub fn packet_samples(packet: &[u8]) -> Result<u32, Box<str>> {
    let toc = *packet.first().ok_or("empty Opus packet")?;
    let config = toc >> 3;
    let frame_samples = match config {
        0..=11 => [480, 960, 1_920, 2_880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => 120 << (config & 3),
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1).ok_or("missing Opus frame count")? & 0x3f),
    };
    let samples = frame_samples * frames;
    if frames == 0 || samples > 5_760 {
        return Err("Opus packet duration exceeds 120 ms or is empty".into());
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use broadcast_common::Serialize;

    #[test]
    fn head_preserves_priming_gain_and_mapping() -> Result<(), Box<str>> {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[
            1, 6, 0x38, 1, 0x44, 0xac, 0, 0, 0, 0xff, 1, 4, 2, 0, 4, 1, 2, 3, 5,
        ]);
        let config = configuration(&head)?;
        assert_eq!(config.version, 0);
        assert_eq!(config.output_channel_count, 6);
        assert_eq!(config.pre_skip, 312);
        assert_eq!(config.input_sample_rate, 44_100);
        assert_eq!(config.output_gain, -256);
        assert_eq!(configuration(&config.to_bytes())?, config);
        head.pop();
        assert!(configuration(&head).is_err());
        Ok(())
    }

    #[test]
    fn durations_cover_silk_hybrid_celt_and_multiple_frames() -> Result<(), Box<str>> {
        for (packet, samples) in [
            (&[0][..], 480),
            (&[24][..], 2_880),
            (&[104][..], 960),
            (&[128][..], 120),
            (&[152][..], 960),
            (&[155, 6][..], 5_760),
        ] {
            assert_eq!(packet_samples(packet)?, samples);
        }
        assert!(packet_samples(&[155, 7]).is_err());
        assert!(packet_samples(&[155, 0]).is_err());
        assert!(packet_samples(&[]).is_err());
        Ok(())
    }
}
