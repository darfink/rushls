//! Video properties recovered from codec headers, shared by ingest and muxing.

use crate::domain::Codec;
use broadcast_common::Parse;

#[derive(Default)]
pub struct VideoProperties {
    pub reorder_depth: u32,
    pub mastering_display: Option<Vec<u8>>,
    pub content_light: Option<Vec<u8>>,
    pub aspect: Option<(u16, u16)>,
    pub colour: Option<(u8, u8, u8, bool)>,
}

pub fn properties(codec: Codec, extra: &[u8]) -> VideoProperties {
    match codec {
        Codec::H264 => avc(extra).unwrap_or_default(),
        Codec::Hevc => hevc(extra).unwrap_or_default(),
        _ => VideoProperties::default(),
    }
}

fn avc(extra: &[u8]) -> Option<VideoProperties> {
    let record = transmux::AVCDecoderConfigurationRecord::parse(extra).ok()?;
    let nal = record.sps.first()?;
    let rbsp = h264_reader::rbsp::decode_nal(&nal.0[..]).ok()?;
    let sps = h264_reader::nal::sps::SeqParameterSet::from_bits(h264_reader::rbsp::BitReader::new(
        rbsp.as_ref(),
    ))
    .ok()?;
    let vui = sps.vui_parameters?;
    Some(VideoProperties {
        mastering_display: None,
        content_light: None,
        reorder_depth: vui
            .bitstream_restrictions
            .map_or(0, |value| value.max_num_reorder_frames),
        aspect: vui.aspect_ratio_info.and_then(|value| value.get()),
        colour: vui.video_signal_type.map(|signal| {
            let description = signal.colour_description;
            (
                description
                    .as_ref()
                    .map_or(2, |value| value.colour_primaries),
                description
                    .as_ref()
                    .map_or(2, |value| value.transfer_characteristics),
                description
                    .as_ref()
                    .map_or(2, |value| value.matrix_coefficients),
                signal.video_full_range_flag,
            )
        }),
    })
}

fn hevc(extra: &[u8]) -> Option<VideoProperties> {
    let record = transmux::HEVCDecoderConfigurationRecord::parse(extra).ok()?;
    let sps = record
        .arrays
        .iter()
        .flat_map(|array| &array.nalus)
        .find_map(|nal| scuffle_h265::SpsNALUnit::parse(&nal.0[..]).ok())?;
    let reorder_depth = u32::try_from(
        *sps.rbsp
            .sub_layer_ordering_info
            .sps_max_num_reorder_pics
            .iter()
            .max()?,
    )
    .ok()?;
    let Some(vui) = sps.rbsp.vui_parameters else {
        return Some(VideoProperties {
            reorder_depth,
            ..VideoProperties::default()
        });
    };
    let aspect = match vui.aspect_ratio_info {
        scuffle_h265::AspectRatioInfo::ExtendedSar {
            sar_width,
            sar_height,
        } => Some((sar_width, sar_height)),
        scuffle_h265::AspectRatioInfo::Predefined(id) => {
            const RATIOS: [(u16, u16); 17] = [
                (0, 0),
                (1, 1),
                (12, 11),
                (10, 11),
                (16, 11),
                (40, 33),
                (24, 11),
                (20, 11),
                (32, 11),
                (80, 33),
                (18, 11),
                (15, 11),
                (64, 33),
                (160, 99),
                (4, 3),
                (3, 2),
                (2, 1),
            ];
            RATIOS.get(usize::from(u8::from(id))).copied()
        }
    }
    .filter(|(width, height)| *width > 0 && *height > 0);
    Some(VideoProperties {
        mastering_display: None,
        content_light: None,
        reorder_depth,
        aspect,
        colour: Some((
            vui.video_signal_type.colour_primaries,
            vui.video_signal_type.transfer_characteristics,
            vui.video_signal_type.matrix_coeffs,
            vui.video_signal_type.video_full_range_flag,
        )),
    })
}

impl VideoProperties {
    /// Static HDR SEI payloads use the same field order and units as mdcv/clli.
    /// Read only before init emission; a live HDR change needs a new init epoch.
    pub fn observe_hdr(&mut self, codec: Codec, length_bytes: usize, payload: &[u8]) {
        let mut bytes = payload;
        while bytes.len() >= length_bytes && (1..=4).contains(&length_bytes) {
            let length = bytes[..length_bytes]
                .iter()
                .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
            let Some(nal) = bytes.get(length_bytes..length_bytes.saturating_add(length)) else {
                return;
            };
            bytes = &bytes[length_bytes + length..];
            let skip = match codec {
                Codec::H264 if nal.first().is_some_and(|byte| byte & 31 == 6) => 1,
                Codec::Hevc
                    if nal
                        .first()
                        .is_some_and(|byte| matches!((byte >> 1) & 63, 39 | 40))
                        && nal.len() >= 2 =>
                {
                    2
                }
                _ => continue,
            };
            let mut rbsp = Vec::with_capacity(nal.len());
            let mut zeros = 0;
            for &byte in &nal[skip..] {
                if zeros >= 2 && byte == 3 {
                    zeros = 0;
                    continue;
                }
                rbsp.push(byte);
                zeros = if byte == 0 { zeros + 1 } else { 0 };
            }
            let mut remaining = &rbsp[..];
            while !remaining.is_empty() && remaining != [0x80] {
                let Some(kind) = sei_number(&mut remaining) else {
                    break;
                };
                let Some(size) = sei_number(&mut remaining) else {
                    break;
                };
                let Some(body) = remaining.get(..size) else {
                    break;
                };
                match (kind, size) {
                    (137, 24) => self.mastering_display = Some(body.to_vec()),
                    (144, 4) => self.content_light = Some(body.to_vec()),
                    _ => {}
                }
                remaining = &remaining[size..];
            }
        }
    }
}
fn sei_number(bytes: &mut &[u8]) -> Option<usize> {
    let mut number = 0usize;
    loop {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        number = number.checked_add(usize::from(byte))?;
        if byte != 255 {
            return Some(number);
        }
    }
}

/// H.264 VUI timing counts fields: a frame spans two clock ticks.
pub fn h264_frame_rate(units: Option<u32>, scale: Option<u32>) -> Option<crate::domain::FrameRate> {
    let units = std::num::NonZeroU32::new(units?)?;
    let scale = std::num::NonZeroU32::new(scale?)?;
    Some(crate::domain::FrameRate::new(
        scale,
        units.checked_mul(nz::u32!(2))?,
    ))
}

/// HEVC VUI timing counts complete frames.
pub fn hevc_frame_rate(units: Option<u32>, scale: Option<u32>) -> Option<crate::domain::FrameRate> {
    Some(crate::domain::FrameRate::new(
        std::num::NonZeroU32::new(scale?)?,
        std::num::NonZeroU32::new(units?)?,
    ))
}
