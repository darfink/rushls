//! Resolve the active PPS and one displayed progressive picture per AVC/HEVC access unit.
use super::cadence_hevc::Bits;
use crate::domain::{CadenceUnavailable as U, Codec};
use broadcast_common::{Parse, bits::BitReader};

pub struct PictureMapping {
    codec: Codec,
    width: usize,
    pps: Vec<u64>,
    unavailable: Option<U>,
}
fn rbsp(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0;
    for &byte in bytes {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}
impl PictureMapping {
    pub fn new(codec: Codec, extra: &[u8]) -> Option<Self> {
        let (width, pps, sps_id) = match codec {
            Codec::H264 => {
                let record = transmux::AVCDecoderConfigurationRecord::parse(extra).ok()?;
                let sps = record.sps.first()?;
                let bytes = rbsp(sps.0.get(1..)?);
                let mut bits = Bits(BitReader::new(&bytes));
                bits.skip(24)?;
                (
                    usize::from(record.length_size_minus_one + 1),
                    record
                        .pps
                        .iter()
                        .filter_map(|p| p.0.get(1..).map(<[u8]>::to_vec))
                        .collect::<Vec<_>>(),
                    bits.ue()?,
                )
            }
            Codec::Hevc => {
                let record = transmux::HEVCDecoderConfigurationRecord::parse(extra).ok()?;
                let sps = record
                    .arrays
                    .iter()
                    .flat_map(|a| &a.nalus)
                    .find_map(|n| scuffle_h265::SpsNALUnit::parse(&n.0[..]).ok())?;
                (
                    usize::from(record.length_size_minus_one + 1),
                    record
                        .arrays
                        .iter()
                        .flat_map(|a| &a.nalus)
                        .filter(|n| n.0.first().is_some_and(|b| (b >> 1) & 63 == 34))
                        .filter_map(|n| n.0.get(2..).map(<[u8]>::to_vec))
                        .collect::<Vec<_>>(),
                    sps.rbsp.sps_seq_parameter_set_id,
                )
            }
            _ => return None,
        };
        let mut this = Self {
            codec,
            width,
            pps: Vec::new(),
            unavailable: None,
        };
        for pps in pps {
            let bytes = rbsp(&pps);
            let mut bits = Bits(BitReader::new(&bytes));
            let id = bits.ue();
            let reference = bits.ue();
            if reference != Some(sps_id) || id.is_none() {
                this.unavailable = Some(U::ParameterSets);
                continue;
            }
            this.pps.push(id.expect("checked id"));
            if codec == Codec::Hevc {
                // output_flag_present_flag would require full slice parsing to
                // distinguish decoded pictures from displayed pictures.
                if bits.flag().is_none() || bits.flag() != Some(false) {
                    this.unavailable = Some(U::DisplayMapping);
                }
            }
        }
        if this.pps.is_empty() {
            this.unavailable = Some(U::ParameterSets);
        }
        Some(this)
    }
    pub fn declaration_unavailable(&self) -> Option<U> {
        self.unavailable
    }
    pub fn check(&self, payload: &[u8]) -> Result<(), U> {
        if let Some(reason) = self.unavailable {
            return Err(reason);
        }
        let mut bytes = payload;
        let mut pictures = 0;
        while !bytes.is_empty() {
            let header = bytes.get(..self.width).ok_or(U::DisplayMapping)?;
            let length = header
                .iter()
                .fold(0usize, |n, b| (n << 8) | usize::from(*b));
            let end = self.width.checked_add(length).ok_or(U::DisplayMapping)?;
            let nal = bytes.get(self.width..end).ok_or(U::DisplayMapping)?;
            bytes = &bytes[end..];
            let first = *nal.first().ok_or(U::DisplayMapping)?;
            let pps = if self.codec == Codec::H264 {
                if !matches!(first & 31, 1..=5) {
                    continue;
                }
                if !matches!(first & 31, 1 | 5) {
                    return Err(U::DisplayMapping);
                }
                let data = rbsp(&nal[1..nal.len().min(65)]);
                let mut bits = Bits(BitReader::new(&data));
                if bits.ue().ok_or(U::DisplayMapping)? == 0 {
                    pictures += 1;
                }
                bits.ue().ok_or(U::DisplayMapping)?;
                bits.ue().ok_or(U::ParameterSets)?
            } else {
                let kind = (first >> 1) & 63;
                if kind > 31 {
                    continue;
                }
                let second = *nal.get(1).ok_or(U::DisplayMapping)?;
                if first & 1 != 0 || second >> 3 != 0 || second & 7 != 1 {
                    return Err(U::TemporalLayers);
                }
                let data = rbsp(&nal[2..nal.len().min(66)]);
                let mut bits = Bits(BitReader::new(&data));
                if bits.flag().ok_or(U::DisplayMapping)? {
                    pictures += 1;
                }
                if (16..=23).contains(&kind) {
                    bits.flag().ok_or(U::DisplayMapping)?;
                }
                bits.ue().ok_or(U::ParameterSets)?
            };
            if !self.pps.contains(&pps) {
                return Err(U::ParameterSets);
            }
        }
        if pictures != 1 {
            return Err(U::DisplayMapping);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::fixtures::{H264_FIXED_CADENCE, HEVC_FIXED_CADENCE};
    #[test]
    fn cadence_mapping_checks_active_parameter_set_layer_and_picture_count() {
        let avc = PictureMapping::new(Codec::H264, H264_FIXED_CADENCE).expect("valid config");
        // first_mb=0, slice_type=0, pps=0; the header alone suffices for mapping.
        assert_eq!(avc.check(&[0, 0, 0, 2, 0x41, 0xe0]), Ok(()));
        assert_eq!(avc.check(&[0, 0, 0, 2, 0x41, 0xd0]), Err(U::ParameterSets));
        assert_eq!(
            avc.check(&[0, 0, 0, 2, 0x41, 0xe0, 0, 0, 0, 2, 0x41, 0xe0]),
            Err(U::DisplayMapping)
        );
        let hevc = PictureMapping::new(Codec::Hevc, HEVC_FIXED_CADENCE).expect("valid config");
        assert_eq!(hevc.check(&[0, 0, 0, 3, 2, 1, 0xc0]), Ok(()));
        assert_eq!(
            hevc.check(&[0, 0, 0, 3, 2, 2, 0xc0]),
            Err(U::TemporalLayers)
        );
        assert_eq!(
            hevc.check(&[0, 0, 0, 3, 2, 9, 0xc0]),
            Err(U::TemporalLayers)
        );
        assert_eq!(hevc.check(&[0, 0, 0, 3, 2, 1, 0xa0]), Err(U::ParameterSets));
        assert_eq!(
            hevc.check(&[0, 0, 0, 4, 2, 1, 0xc0]),
            Err(U::DisplayMapping)
        );
    }
}
