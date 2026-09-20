//! Codec declarations are evidence of intended cadence, not proof of valid timestamps.
use crate::domain::{
    CadenceSource as S, CadenceUnavailable as U, Codec, DecoderConfigOrigin, DiscoveredTrack,
    FrameRate, MediaParameters, VideoCadence as C,
};
use broadcast_common::Parse;

pub fn inspect(track: &DiscoveredTrack) -> C {
    let MediaParameters::Video { frame_rate, .. } = track.parameters else {
        return C::Unknown;
    };
    let declaration = match track.codec {
        Codec::H264 => avc(track.codec_extradata.as_bytes()),
        Codec::Hevc => hevc(
            track.codec_extradata.as_bytes(),
            track.decoder_config_origin,
        ),
        Codec::Av1 => av1(track.codec_extradata.as_bytes()),
        _ => C::Unknown,
    };
    let declaration = if matches!(declaration, C::Fixed { .. }) {
        super::picture_mapping::PictureMapping::new(track.codec, track.codec_extradata.as_bytes())
            .and_then(|mapping| mapping.declaration_unavailable())
            .map_or(declaration, |reason| C::Unverifiable {
                rate: declaration.rate(),
                source: declaration.source().expect("fixed source"),
                reason,
            })
    } else {
        declaration
    };
    if declaration == C::Unknown {
        frame_rate.map_or(C::Unknown, C::Nominal)
    } else {
        declaration
    }
}
/// Metadata rates are approximate hints and never a fixed-cadence contract.
pub fn nominal_rate(value: f64) -> Option<FrameRate> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let numerator = format!("{value:.6}").replace('.', "").parse::<u64>().ok()?;
    rate(numerator, 1_000_000)
}
pub(super) fn rate(num: u64, den: u64) -> Option<FrameRate> {
    if num == 0 || den == 0 {
        return None;
    }
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Some(FrameRate::new(
        std::num::NonZeroU32::new(u32::try_from(num / a).ok()?)?,
        std::num::NonZeroU32::new(u32::try_from(den / a).ok()?)?,
    ))
}
pub(super) fn fixed(rate: Option<FrameRate>, source: S) -> C {
    rate.map_or(
        C::Unverifiable {
            rate: None,
            source,
            reason: U::InvalidTiming,
        },
        |rate| C::Fixed {
            rate,
            source,
            scope: match source {
                S::H264Vui => crate::domain::CadenceScope::ProgressiveFrames,
                S::Av1Sequence => crate::domain::CadenceScope::SingleLayerTemporalUnits,
                _ => crate::domain::CadenceScope::ProgressiveBaseLayer,
            },
        },
    )
}
// Declaration precedence is significant: a conflict must dominate unverified scope.
#[allow(clippy::match_same_arms, clippy::unnested_or_patterns)]
pub(super) fn merge(a: C, b: C) -> C {
    match (a, b) {
        (C::Unknown, b) | (b, C::Unknown) => b,
        (
            C::Fixed {
                rate: a, source, ..
            },
            C::Fixed { rate: b, .. },
        ) if a != b => C::Conflicting { source },
        (a @ C::Conflicting { .. }, _)
        | (_, a @ C::Conflicting { .. })
        | (a @ C::Unverifiable { .. }, _)
        | (_, a @ C::Unverifiable { .. }) => a,
        (a, _) => a,
    }
}
fn avc(extra: &[u8]) -> C {
    let Ok(record) = transmux::AVCDecoderConfigurationRecord::parse(extra) else {
        return C::Unknown;
    };
    let mut result = C::Unknown;
    for nal in &record.sps {
        let Ok(rbsp) = h264_reader::rbsp::decode_nal(&nal.0) else {
            continue;
        };
        let Ok(sps) = h264_reader::nal::sps::SeqParameterSet::from_bits(
            h264_reader::rbsp::BitReader::new(rbsp.as_ref()),
        ) else {
            continue;
        };
        let Some(vui) = sps.vui_parameters else {
            continue;
        };
        let Some(timing) = vui.timing_info else {
            continue;
        };
        if !timing.fixed_frame_rate_flag {
            continue;
        }
        let declaration = if !matches!(
            sps.frame_mbs_flags,
            h264_reader::nal::sps::FrameMbsFlags::Frames
        ) || vui.pic_struct_present_flag
        {
            C::Unverifiable {
                rate: rate(
                    u64::from(timing.time_scale),
                    u64::from(timing.num_units_in_tick) * 2,
                ),
                source: S::H264Vui,
                reason: U::PictureStructure,
            }
        } else {
            fixed(
                rate(
                    u64::from(timing.time_scale),
                    u64::from(timing.num_units_in_tick) * 2,
                ),
                S::H264Vui,
            )
        };
        result = merge(result, declaration);
    }
    // Multiple active parameter-set mappings need slice-level selection. Never
    // silently pick the first SPS when its contract might not apply.
    if record.sps.len() != 1 && matches!(result, C::Fixed { .. }) {
        return C::Unverifiable {
            rate: result.rate(),
            source: S::H264Vui,
            reason: U::ParameterSets,
        };
    }
    result
}
fn hevc(extra: &[u8], origin: DecoderConfigOrigin) -> C {
    let Ok(record) = transmux::HEVCDecoderConfigurationRecord::parse(extra) else {
        return C::Unknown;
    };
    let mut result = C::Unknown;
    let mut sps_count = 0;
    let mut vps_ids = Vec::new();
    let mut sps_vps_ids = Vec::new();
    for nal in record.arrays.iter().flat_map(|a| &a.nalus) {
        if nal.0.first().is_some_and(|byte| (byte >> 1) & 63 == 32) {
            if let Some(id) = nal.0.get(2) {
                vps_ids.push(id >> 4);
            }
            result = merge(result, super::cadence_hevc::vps(&nal.0));
        }
        let Ok(sps) = scuffle_h265::SpsNALUnit::parse(&nal.0[..]) else {
            continue;
        };
        sps_count += 1;
        sps_vps_ids.push(sps.rbsp.sps_video_parameter_set_id);
        let Some(vui) = sps.rbsp.vui_parameters else {
            continue;
        };
        let Some(timing) = vui.vui_timing_info else {
            continue;
        };
        let Some(hrd) = timing.hrd_parameters else {
            continue;
        };
        let mut local = C::Unknown;
        for layer in &hrd.sub_layers {
            if layer.fixed_pic_rate_general_flag || layer.fixed_pic_rate_within_cvs_flag {
                let interval = layer
                    .elemental_duration_in_tc_minus1
                    .and_then(|value| value.checked_add(1))
                    .and_then(|value| value.checked_mul(u64::from(timing.num_units_in_tick.get())));
                local = merge(
                    local,
                    fixed(
                        interval.and_then(|den| rate(u64::from(timing.time_scale.get()), den)),
                        S::HevcSpsHrd,
                    ),
                );
            }
        }
        // Different rates at different temporal layers are legitimate, not
        // contradictory. This implementation cannot select a sub-bitstream.
        if (sps.rbsp.sps_max_sub_layers_minus1 != 0 || sps.nal_unit_header.nuh_layer_id != 0)
            && local != C::Unknown
        {
            local = C::Unverifiable {
                rate: local.rate(),
                source: S::HevcSpsHrd,
                reason: U::TemporalLayers,
            };
        }
        if (vui.field_seq_flag || vui.frame_field_info_present_flag) && local != C::Unknown {
            local = C::Unverifiable {
                rate: local.rate(),
                source: S::HevcSpsHrd,
                reason: U::PictureStructure,
            };
        }
        result = merge(result, local);
    }
    if (sps_count != 1
        || vps_ids.len() > 1
        || (!vps_ids.is_empty() && sps_vps_ids.iter().any(|id| !vps_ids.contains(id))))
        && matches!(result, C::Fixed { .. })
    {
        result = C::Unverifiable {
            rate: result.rate(),
            source: S::HevcSpsHrd,
            reason: U::ParameterSets,
        };
    }
    hevc_container_evidence(
        result,
        record.constant_frame_rate,
        record.num_temporal_layers,
        origin,
    )
}
fn hevc_container_evidence(
    mut result: C,
    constant: u8,
    layers: u8,
    origin: DecoderConfigOrigin,
) -> C {
    if origin == DecoderConfigOrigin::Publisher {
        match constant {
            1 if result == C::Unknown => {
                result = C::Unverifiable {
                    rate: None,
                    source: S::HevcConfiguration,
                    reason: U::MissingExactInterval,
                }
            }
            // Per-layer CFR is unambiguous when HRD already resolved a single layer.
            2 if layers <= 1 && matches!(result, C::Fixed { .. }) => {}
            2 => {
                result = merge(
                    result,
                    C::Unverifiable {
                        rate: None,
                        source: S::HevcConfiguration,
                        reason: U::TemporalLayers,
                    },
                );
            }
            3 => {
                result = C::Conflicting {
                    source: S::HevcConfiguration,
                }
            }
            _ => {}
        }
    }
    result
}
fn av1(extra: &[u8]) -> C {
    av1_headers(extra).unwrap_or(C::Unknown)
}

fn av1_headers(extra: &[u8]) -> Option<C> {
    let bytes = extra.get(4..)?;
    let mut cursor = std::io::Cursor::new(bytes);
    let mut result = C::Unknown;
    while usize::try_from(cursor.position()).ok()? < bytes.len() {
        let header = scuffle_av1::ObuHeader::parse(&mut cursor).ok()?;
        let start = usize::try_from(cursor.position()).ok()?;
        let end = start.checked_add(usize::try_from(header.size?).ok()?)?;
        let body = bytes.get(start..end)?;
        if header.obu_type == scuffle_av1::ObuType::SequenceHeader {
            let sequence =
                scuffle_av1::seq::SequenceHeaderObu::parse(header, &mut &body[..]).ok()?;
            result = merge(result, av1_sequence(&sequence));
        }
        cursor.set_position(u64::try_from(end).ok()?);
    }
    Some(result)
}
fn av1_sequence(sequence: &scuffle_av1::seq::SequenceHeaderObu) -> C {
    let Some(timing) = &sequence.timing_info else {
        return C::Unknown;
    };
    let Some(ticks) = timing.num_ticks_per_picture else {
        return C::Unknown;
    };
    if sequence.operating_points.len() != 1 || sequence.operating_points[0].idc != 0 {
        return C::Unverifiable {
            rate: None,
            source: S::Av1Sequence,
            reason: U::TemporalLayers,
        };
    }
    fixed(
        ticks
            .checked_mul(u64::from(timing.num_units_in_display_tick))
            .and_then(|den| rate(u64::from(timing.time_scale), den)),
        S::Av1Sequence,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaKind, fixtures::TrackBuilder};
    use std::fmt::Write;
    // FFmpeg 8 encoder headers from 64x64 color at 25 fps. H.264: libx264
    // force-cfr=1; HEVC: libx265 hrd=1 with VBV; AV1: SVT-AV1 with
    // av1_metadata=tick_rate=25:num_ticks_per_picture=1. No sample payloads.
    use crate::media::fixtures::AV1_FIXED_CADENCE as AV1;
    use crate::media::fixtures::H264_FIXED_CADENCE as H264;
    use crate::media::fixtures::HEVC_FIXED_CADENCE as HEVC;

    #[test]
    fn real_encoder_headers_provide_exact_fixed_cadence_across_origins() {
        for (codec, bytes) in [(Codec::H264, H264), (Codec::Hevc, HEVC), (Codec::Av1, AV1)] {
            for origin in [
                DecoderConfigOrigin::Publisher,
                DecoderConfigOrigin::Synthesized,
            ] {
                let mut track = TrackBuilder::new(0, MediaKind::Video)
                    .codec(codec)
                    .codec_extradata(bytes)
                    .build();
                track.decoder_config_origin = origin;
                let cadence = inspect(&track);
                assert!(
                    matches!(cadence, C::Fixed { .. }),
                    "{codec:?} {origin:?}: {cadence:?}"
                );
                assert_eq!(cadence.rate(), rate(25, 1));
            }
        }
    }
    #[test]
    fn synthesized_configuration_is_not_publisher_constant_rate_evidence() {
        let mut bytes = HEVC[..23].to_vec();
        bytes[22] = 0;
        bytes[21] = (bytes[21] & 63) | 64;
        assert!(matches!(
            hevc(&bytes, DecoderConfigOrigin::Publisher),
            C::Unverifiable {
                reason: U::MissingExactInterval,
                ..
            }
        ));
        assert_eq!(hevc(&bytes, DecoderConfigOrigin::Synthesized), C::Unknown);
        bytes[21] = (bytes[21] & 63) | 128;
        assert!(matches!(
            hevc(&bytes, DecoderConfigOrigin::Publisher),
            C::Unverifiable {
                reason: U::TemporalLayers,
                ..
            }
        ));
    }
    #[test]
    fn contradictory_rates_are_not_resolved_by_choosing_one() {
        assert!(matches!(
            merge(
                fixed(rate(25, 1), S::HevcSpsHrd),
                fixed(rate(30, 1), S::HevcVpsHrd)
            ),
            C::Conflicting { .. }
        ));
        assert!(matches!(
            merge(
                fixed(rate(25, 1), S::H264Vui),
                fixed(rate(50, 2), S::H264Vui)
            ),
            C::Fixed { .. }
        ));
    }
    #[test]
    fn truncated_headers_do_not_panic_or_fabricate_fixed_intervals() {
        for (codec, bytes) in [(Codec::H264, H264), (Codec::Hevc, HEVC), (Codec::Av1, AV1)] {
            for n in 0..bytes.len() {
                let track = TrackBuilder::new(0, MediaKind::Video)
                    .codec(codec)
                    .codec_extradata(&bytes[..n])
                    .build();
                let _ = inspect(&track);
            }
        }
    }
    fn avc_fixture(fixed: bool, fields: bool, pic_struct: bool) -> Vec<u8> {
        let mut bits = String::from("010000100000000000011110");
        // SPS id, frame_num, POC type and POC lsb size; one reference picture.
        bits.push_str("11110100");
        bits.push_str("0010000100");
        bits.push(if fields { '0' } else { '1' });
        if fields {
            bits.push('0');
        }
        bits.push_str("10100001");
        write!(bits, "{:032b}{:032b}", 1001, 60000).expect("String writes cannot fail");
        bits.push(if fixed { '1' } else { '0' });
        bits.push_str("00");
        bits.push(if pic_struct { '1' } else { '0' });
        bits.push_str("01");
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let mut nal = vec![0x67];
        let mut zeros = 0;
        for chunk in bits.as_bytes().as_chunks::<8>().0 {
            let byte = chunk.iter().fold(0, |n, b| (n << 1) | u8::from(*b == b'1'));
            if zeros >= 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            nal.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        let mut bytes = vec![1, 66, 0, 30, 255, 225];
        bytes.extend_from_slice(&(u16::try_from(nal.len()).expect("small SPS")).to_be_bytes());
        bytes.extend(nal);
        bytes.extend_from_slice(&[1, 0, 4, 0x68, 0xce, 0x3c, 0x80]);
        bytes
    }
    #[test]
    fn avc_fixed_flag_and_picture_structure_control_validation() {
        assert_eq!(avc(&avc_fixture(false, false, false)), C::Unknown);
        let c = avc(&avc_fixture(true, false, false));
        assert!(matches!(c, C::Fixed { .. }), "{c:?}");
        assert_eq!(c.rate(), rate(30000, 1001));
        for (fields, pic_struct) in [(true, false), (false, true)] {
            assert!(matches!(
                avc(&avc_fixture(true, fields, pic_struct)),
                C::Unverifiable {
                    reason: U::PictureStructure,
                    ..
                }
            ));
        }
    }
    #[test]
    fn contradictory_av1_sequence_headers_are_not_hidden_by_the_first_header() {
        let mut second = AV1.to_vec();
        // Sequence timing follows profile/still/reduced/timing-present (6 bits)
        // and num_units_in_display_tick (32 bits). Replace time_scale by 50.
        for bit in 0..32 {
            let position = 38 + bit;
            let mask = 1u8 << (7 - position % 8);
            let value = u8::from((50u32 >> (31 - bit)) & 1 != 0);
            second[6 + position / 8] =
                (second[6 + position / 8] & !mask) | (value << (7 - position % 8));
        }
        assert_eq!(av1(&second).rate(), rate(50, 1));
        let mut combined = AV1.to_vec();
        combined.extend_from_slice(&second[4..]);
        assert!(matches!(av1(&combined), C::Conflicting { .. }));
        let mut repeated = AV1.to_vec();
        repeated.extend_from_slice(&AV1[4..]);
        assert_eq!(av1(&repeated).rate(), rate(25, 1));
    }
    #[test]
    fn per_layer_constant_rate_is_verifiable_for_a_resolved_single_layer() {
        let mut bytes = HEVC.to_vec();
        bytes[21] = (bytes[21] & 63) | 128;
        assert!(matches!(
            hevc(&bytes, DecoderConfigOrigin::Publisher),
            C::Fixed { .. }
        ));
        bytes[21] = (bytes[21] & !0x38) | (2 << 3);
        assert!(matches!(
            hevc(&bytes, DecoderConfigOrigin::Publisher),
            C::Unverifiable {
                reason: U::TemporalLayers,
                ..
            }
        ));
    }
}
