//! VPS timing syntax not exposed by the SPS parser dependency.
use crate::domain::{CadenceSource as S, CadenceUnavailable as U, VideoCadence as C};
use broadcast_common::bits::BitReader;
pub(super) struct Bits<'a>(pub(super) BitReader<'a>);
impl Bits<'_> {
    pub(super) fn n(&mut self, n: u32) -> Option<u64> {
        self.0.read_bits(n).ok()
    }
    pub(super) fn skip(&mut self, n: usize) -> Option<()> {
        self.0.skip_bits(n).ok()
    }
    pub(super) fn flag(&mut self) -> Option<bool> {
        self.0.read_bool().ok()
    }
    pub(super) fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1) + self.n(zeros)?)
    }
}
pub fn vps(nal: &[u8]) -> C {
    let Some(body) = nal.get(2..) else {
        return C::Unknown;
    };
    let mut bytes = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for &byte in body {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        bytes.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    parse(&mut Bits(BitReader::new(&bytes))).unwrap_or(C::Unverifiable {
        rate: None,
        source: S::HevcVpsHrd,
        reason: U::InvalidTiming,
    })
}
// Kept in syntax order so bit consumption can be audited against H.265.
#[allow(clippy::too_many_lines)]
fn parse(b: &mut Bits<'_>) -> Option<C> {
    b.skip(6)?;
    let layers = b.n(6)?;
    let sublayers = usize::try_from(b.n(3)?).ok()?;
    b.skip(17)?;
    b.skip(96)?; // General profile_tier_level.
    let mut profiles = Vec::new();
    for _ in 0..sublayers {
        profiles.push((b.flag()?, b.flag()?));
    }
    if sublayers > 0 {
        b.skip((8 - sublayers) * 2)?;
    }
    for (profile, level) in profiles {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    let ordering = b.flag()?;
    for _ in if ordering { 0 } else { sublayers }..=sublayers {
        b.ue()?;
        b.ue()?;
        b.ue()?;
    }
    let max_layer = usize::try_from(b.n(6)?).ok()?;
    let sets = usize::try_from(b.ue()?).ok()?;
    if sets > 1023 {
        return None;
    }
    b.skip(sets.checked_mul(max_layer + 1)?)?;
    if !b.flag()? {
        return Some(C::Unknown);
    }
    let units = b.n(32)?;
    let scale = b.n(32)?;
    if b.flag()? {
        b.ue()?;
    }
    let count = b.ue()?;
    if count > 1024 {
        return None;
    }
    let mut result = C::Unknown;
    let mut common = (false, false, false);
    for i in 0..count {
        let set = b.ue()?;
        let present = i == 0 || b.flag()?;
        if present {
            let nal = b.flag()?;
            let vcl = b.flag()?;
            let sub = if nal || vcl { b.flag()? } else { false };
            if sub {
                b.skip(19)?;
            }
            if nal || vcl {
                b.skip(8)?;
                if sub {
                    b.skip(4)?;
                }
                b.skip(15)?;
            }
            common = (nal, vcl, sub);
        }
        for _ in 0..=sublayers {
            let general = b.flag()?;
            let fixed = general || b.flag()?;
            let duration = if fixed {
                Some(b.ue()?.checked_add(1)?)
            } else {
                None
            };
            let low_delay = if fixed { false } else { b.flag()? };
            let cpbs = if low_delay { 0 } else { b.ue()? };
            if cpbs > 31 {
                return None;
            }
            for enabled in [common.0, common.1] {
                if enabled {
                    for _ in 0..=cpbs {
                        b.ue()?;
                        b.ue()?;
                        if common.2 {
                            b.ue()?;
                            b.ue()?;
                        }
                        b.flag()?;
                    }
                }
            }
            if let Some(duration) = duration {
                let next = if layers != 0 || sublayers != 0 || set != 0 {
                    C::Unverifiable {
                        rate: None,
                        source: S::HevcVpsHrd,
                        reason: U::TemporalLayers,
                    }
                } else {
                    super::cadence::fixed(
                        units
                            .checked_mul(duration)
                            .and_then(|den| super::cadence::rate(scale, den)),
                        S::HevcVpsHrd,
                    )
                };
                result = super::cadence::merge(result, next);
            }
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;
    fn vps_fixture(fixed: bool, sublayers: u8) -> Vec<u8> {
        // H.265 VPS syntax: one layer set, one common HRD, no CPB entries.
        let mut bits = String::new();
        bits.push_str("000011000000");
        write!(bits, "{sublayers:03b}").expect("String writes cannot fail");
        bits.push('1');
        bits.push_str("1111111111111111");
        bits.push_str(&"0".repeat(96));
        if sublayers > 0 {
            bits.push_str(&"0".repeat(16));
        }
        bits.push('0');
        bits.push_str("111");
        bits.push_str("0000001");
        bits.push('1');
        write!(bits, "{:032b}{:032b}", 1, 25).expect("String writes cannot fail");
        bits.push('0');
        bits.push_str("010");
        bits.push('1');
        bits.push_str("00");
        for _ in 0..=sublayers {
            bits.push_str(if fixed { "111" } else { "001" });
        }
        bits.push('1');
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let mut bytes = vec![0x40, 1];
        let mut zeros = 0;
        for chunk in bits.as_bytes().as_chunks::<8>().0 {
            let byte = chunk.iter().fold(0, |n, b| (n << 1) | u8::from(*b == b'1'));
            if zeros >= 2 && byte <= 3 {
                bytes.push(3);
                zeros = 0;
            }
            bytes.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        bytes
    }
    #[test]
    fn vps_hrd_requires_fixed_flag_and_resolved_temporal_scope() {
        assert_eq!(vps(&vps_fixture(false, 0)), C::Unknown);
        assert!(matches!(vps(&vps_fixture(true, 0)), C::Fixed { .. }));
        assert_eq!(
            vps(&vps_fixture(true, 0)).rate(),
            super::super::cadence::rate(25, 1)
        );
        assert!(matches!(
            vps(&vps_fixture(true, 1)),
            C::Unverifiable {
                reason: U::TemporalLayers,
                ..
            }
        ));
    }
}
