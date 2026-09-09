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
        Codec::Hevc => Some(hevc(config)),
        Codec::Av1 => av1(config),
        Codec::Aac => Some(aac(config)),
        Codec::Opus => Some(Arc::from("opus")),
        Codec::WebVtt => Some(Arc::from("wvtt")),
        Codec::MovText => Some(Arc::from("tx3g")),
        // Both are converted to WebVTT before publication, so the input codec
        // never describes what a manifest advertises.
        Codec::SubRip | Codec::Text | Codec::Unknown(_) => None,
    }
}

/// AV1 requires profile, level, tier, and bit depth even in its shortest form.
fn av1(config: Option<&[u8]>) -> Option<Arc<str>> {
    let config = config?;
    if config.len() < 4 || config[0] != 0x81 {
        return None;
    }
    // av1C stores the sequence-header identity in its fixed four-byte header;
    // config OBUs are optional, so no sequence-header payload is needed here.
    let profile = config[1] >> 5;
    let level = config[1] & 0x1f;
    let tier = if config[2] & 0x80 == 0 { 'M' } else { 'H' };
    let high_bitdepth = config[2] & 0x40 != 0;
    let twelve_bit = config[2] & 0x20 != 0;
    if profile > 2 || (twelve_bit && (profile != 2 || !high_bitdepth)) {
        return None;
    }
    let bit_depth = if twelve_bit {
        12
    } else if high_bitdepth {
        10
    } else {
        8
    };
    Some(Arc::from(format!(
        "av01.{profile}.{level:02}{tier}.{bit_depth:02}"
    )))
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
        Arc::from(format!("avc1.{profile:02x}{constraints:02x}{level:02x}"))
    } else {
        Arc::from("avc1")
    }
}

/// \`hvc1.PPP.CCCCCCCC.TLLL.BB...\` from an \`HEVCDecoderConfigurationRecord\`.
///
/// Every component after the sample entry name comes from the record's general
/// profile-tier-level fields, which is the whole reason this takes the bytes:
/// a bare \`hvc1\` tells a player nothing, and Apple's validator reports it as a
/// \`CODECS\` attribute that does not declare the format actually present.
///
/// ISO/IEC 14496-15 annex E fixes the spelling, and it is not the hexadecimal
/// run that H.264 uses:
///
/// * the profile space is \`\`/\`A\`/\`B\`/\`C\` followed by the decimal profile;
/// * the compatibility flags are hexadecimal, *bit-reversed*, with leading
///   zeros dropped — a detail that is easy to miss and produces a string
///   players silently reject;
/// * the tier is \`L\` or \`H\` followed by the decimal level;
/// * the six constraint bytes are hexadecimal, dot-separated, with trailing
///   zero bytes omitted.
///
/// A record that will not parse falls back to the bare name rather than
/// guessing: an inaccurate refinement is worse than none, because a player
/// trusts it enough to refuse the stream without fetching a segment.
fn hevc(config: Option<&[u8]>) -> Arc<str> {
    let Some(record) = config.and_then(|bytes| {
        <transmux::HEVCDecoderConfigurationRecord as broadcast_common::Parse>::parse(bytes).ok()
    }) else {
        return Arc::from("hvc1");
    };

    let space = match record.general_profile_space {
        0 => "",
        1 => "A",
        2 => "B",
        3 => "C",
        _ => return Arc::from("hvc1"),
    };
    let compatibility = record.general_profile_compatibility_flags.reverse_bits();
    let tier = if record.general_tier_flag { 'H' } else { 'L' };

    let mut codec = format!(
        "hvc1.{space}{}.{compatibility:X}.{tier}{}",
        record.general_profile_idc, record.general_level_idc
    );
    // The 48-bit field is stored most-significant byte first. Trailing zero
    // bytes carry no information and annex E omits them, so a Main-profile
    // stream with only the progressive-source flag set spells one byte rather
    // than six.
    let constraints = record.general_constraint_indicator_flags.to_be_bytes();
    let significant = constraints[2..]
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    for byte in &constraints[2..2 + significant] {
        use std::fmt::Write as _;
        let _ = write!(codec, ".{byte:02X}");
    }
    Arc::from(codec)
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
    // Parse backward-compatible SBR/PS extensions with the same reader used
    // for audio timing. A partial configuration retains the old header fallback.
    // AudioSpecificConfig: the first 5 bits are audioObjectType. Type 0 is
    // "null" and never describes real media, so it falls back to 2 (AAC-LC),
    // which is what an encoder that declared nothing is overwhelmingly likely
    // to be producing.
    let audio_object_type = config
        .and_then(|asc| {
            super::aac::audio_object_type(asc)
                .ok()
                .or_else(|| asc.first().map(|first| u32::from(first >> 3)))
        })
        .filter(|value| *value > 0)
        .unwrap_or(2);
    Arc::from(format!("mp4a.40.{audio_object_type}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_declares_the_required_decoder_identity() {
        for (config, expected) in [
            ([0x81, 0x09, 0x0d, 0], "av01.0.09M.08"),
            ([0x81, 0x04, 0x40, 0], "av01.0.04M.10"),
            ([0x81, 0x2d, 0xc0, 0], "av01.1.13H.10"),
            ([0x81, 0x48, 0x60, 0], "av01.2.08M.12"),
        ] {
            assert_eq!(
                rfc6381(Codec::Av1, Some(&config)).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn av1_does_not_advertise_an_incomplete_or_invented_identity() {
        assert_eq!(rfc6381(Codec::Av1, None), None);
        for config in [
            &[][..],
            &[0x81, 9, 13],
            &[1, 9, 13, 0],
            &[0x81, 0x60, 0, 0],
            &[0x81, 0, 0x60, 0],
        ] {
            assert_eq!(rfc6381(Codec::Av1, Some(config)), None);
        }
    }

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
