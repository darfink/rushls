//! RFC 6381 `codecs` strings.
//!
//! One mapping, used by every manifest that has to declare what a player will
//! need to decode: HLS `CODECS`, DASH `@codecs`, and the MP4 `codecs` MIME
//! parameter all spell it the same way.
//!
//! # Why the configuration bytes matter
//!
//! A bare `avc1` is legal but useless: a player that cannot decode High 5.1 has
//! no way to tell from it, so it either downloads the segments to find out or
//! refuses a stream it could have played. The refined form carries profile,
//! constraints, and level, and those live only in the codec configuration
//! record — which is why this takes the bytes rather than the [`Codec`] alone.
//!
//! MP4-family inputs carry H.264 as an `AVCDecoderConfigurationRecord`, while
//! MPEG-TS exposes the same SPS/PPS configuration in Annex B form. AAC uses an
//! `AudioSpecificConfig`. The projection recognizes both H.264 representations
//! without making the domain track claim one container's framing universally.

use std::sync::Arc;

use super::Codec;

/// Maps provider-neutral codec metadata to an RFC 6381 codec string.
///
/// `None` means the codec has no representable string — the caller decides
/// whether that is fatal. Omitting `CODECS` is legal in HLS but costs a player
/// the ability to reject a stream before fetching it.
pub fn rfc6381(codec: Codec, config: Option<&[u8]>) -> Option<Arc<str>> {
    match codec {
        Codec::H264 => Some(h264(config)),
        Codec::Hevc => Some(Arc::from("hvc1")),
        Codec::Av1 => Some(Arc::from("av01")),
        Codec::Aac => Some(aac(config)),
        Codec::Opus => Some(Arc::from("opus")),
        Codec::WebVtt => Some(Arc::from("wvtt")),
        Codec::MovText => Some(Arc::from("tx3g")),
        Codec::SubRip | Codec::Unknown(_) => None,
    }
}

/// `avc1.PPCCLL` from the profile, constraint flags, and level of an avcC.
fn h264(config: Option<&[u8]>) -> Arc<str> {
    let Some(config) = config else {
        return Arc::from("avc1");
    };

    // AVCDecoderConfigurationRecord: [configurationVersion=1,
    // AVCProfileIndication, profile_compatibility, AVCLevelIndication, ...].
    let identity = if config.len() >= 4 && config[0] == 1 {
        Some([config[1], config[2], config[3]])
    } else {
        annex_b_sps_identity(config)
    };
    if let Some([profile, constraints, level]) = identity {
        Arc::from(format!(
            "avc1.{:02x}{:02x}{:02x}",
            profile, constraints, level
        ))
    } else {
        Arc::from("avc1")
    }
}

/// Finds the first SPS and reads the three bytes shared with avcC.
///
/// They immediately follow the SPS NAL header, before fields whose Exp-Golomb
/// coding or emulation-prevention bytes would require a full H.264 parser.
fn annex_b_sps_identity(config: &[u8]) -> Option<[u8; 3]> {
    let mut offset = 0_usize;
    while offset + 4 <= config.len() {
        let start_length = if config[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if config[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        let nal = offset + start_length;
        if config.get(nal).is_some_and(|header| header & 0x1f == 7) {
            return Some([
                *config.get(nal + 1)?,
                *config.get(nal + 2)?,
                *config.get(nal + 3)?,
            ]);
        }
        offset = nal + 1;
    }
    None
}

/// `mp4a.40.N` from the audio object type of an AudioSpecificConfig.
fn aac(config: Option<&[u8]>) -> Arc<str> {
    // AudioSpecificConfig: the first 5 bits are audioObjectType. Type 0 is
    // "null" and never describes real media, so it falls back to 2 (AAC-LC),
    // which is what an encoder that declared nothing is overwhelmingly likely
    // to be producing.
    let audio_object_type = config
        .and_then(|asc| asc.first().copied())
        .map(|first| u32::from(first >> 3))
        .filter(|value| *value > 0)
        .unwrap_or(2);
    Arc::from(format!("mp4a.40.{audio_object_type}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_refines_from_the_configuration_record() {
        // High profile, no constraints, level 4.0.
        let avcc = [1_u8, 0x64, 0x00, 0x28, 0xff];
        assert_eq!(
            rfc6381(Codec::H264, Some(&avcc)).as_deref(),
            Some("avc1.640028")
        );
    }

    #[test]
    fn h264_refines_from_annex_b_sps_configuration() {
        let annex_b = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0, 0, 1, 0x68, 0xee,
        ];
        assert_eq!(
            rfc6381(Codec::H264, Some(&annex_b)).as_deref(),
            Some("avc1.640028")
        );
    }

    #[test]
    fn h264_falls_back_rather_than_advertising_a_guess() {
        let truncated_annex_b = [0_u8, 0x00, 0x00, 0x01, 0x67];
        assert_eq!(
            rfc6381(Codec::H264, Some(&truncated_annex_b)).as_deref(),
            Some("avc1")
        );
        assert_eq!(
            rfc6381(Codec::H264, Some(&[1, 0x64])).as_deref(),
            Some("avc1")
        );
        assert_eq!(rfc6381(Codec::H264, None).as_deref(), Some("avc1"));
    }

    #[test]
    fn aac_reads_the_audio_object_type_and_defaults_to_lc() {
        // audioObjectType 5 (HE-AAC) in the top five bits.
        assert_eq!(
            rfc6381(Codec::Aac, Some(&[0x2b, 0x09])).as_deref(),
            Some("mp4a.40.5")
        );
        assert_eq!(rfc6381(Codec::Aac, None).as_deref(), Some("mp4a.40.2"));
        assert_eq!(
            rfc6381(Codec::Aac, Some(&[0x00])).as_deref(),
            Some("mp4a.40.2"),
            "audio object type 0 is not real media"
        );
    }

    #[test]
    fn an_unrecognized_codec_has_no_representable_string() {
        assert_eq!(rfc6381(Codec::Unknown(42), None), None);
    }
}
