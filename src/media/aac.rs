//! Decoded AAC cadence from AudioSpecificConfig, including explicit SBR/PS.

use crate::domain::{AudioTiming, MediaParameters};
use broadcast_common::bits::BitReader;
use std::num::{NonZeroU16, NonZeroU32};

/// Reads the core rate separately from the SBR output rate. Frame size is
/// expressed in decoded output samples, as required by the audio normalizer.
pub fn parameters(bytes: &[u8]) -> Result<MediaParameters, Box<str>> {
    let mut bits = BitReader::new(bytes);
    let mut object = read(&mut bits, 5)?;
    if object == 31 {
        object = 32 + read(&mut bits, 6)?;
    }
    let core_rate = frequency(&mut bits)?;
    let channel_config = read(&mut bits, 4)?;
    let mut output_rate = core_rate;
    let mut sbr = matches!(object, 5 | 29);
    let mut ps = object == 29;
    if sbr {
        output_rate = frequency(&mut bits)?;
        object = read(&mut bits, 5)?;
    }
    if object != 2 {
        return Err("AAC requires an LC core (with optional SBR/PS)".into());
    }
    let core_samples = if read(&mut bits, 1)? == 0 { 1_024 } else { 960 };
    if read(&mut bits, 1)? != 0 {
        read(&mut bits, 14)?;
    }
    if read(&mut bits, 1)? != 0 {
        return Err("unsupported AAC GASpecificConfig extension".into());
    }
    // Sync extensions follow GASpecificConfig; do not scan arbitrary payload
    // bits for a coincidental sync word and invent an output sample rate.
    if !sbr && bits.bits_remaining() >= 16 && read(&mut bits, 11)? == 0x2b7 {
        if read(&mut bits, 5)? != 5 {
            return Err("unsupported AAC sync extension".into());
        }
        sbr = read(&mut bits, 1)? != 0;
        if sbr {
            output_rate = frequency(&mut bits)?;
        }
        if bits.bits_remaining() >= 12 && read(&mut bits, 11)? == 0x548 {
            ps = read(&mut bits, 1)? != 0;
        }
    }
    if ps && !sbr {
        return Err("AAC PS requires SBR".into());
    }
    if output_rate != core_rate && (!sbr || output_rate != core_rate * 2) {
        return Err("unsupported AAC SBR sample-rate ratio".into());
    }
    let channels = match channel_config {
        1..=6 => channel_config,
        7 => 8,
        _ => return Err("AAC requires an explicit supported channel configuration".into()),
    };
    if ps && channels != 1 {
        return Err("AAC PS requires a mono core".into());
    }
    Ok(MediaParameters::Audio {
        sample_rate: NonZeroU32::new(output_rate).ok_or("zero AAC output rate")?,
        channels: NonZeroU16::new(
            u16::try_from(if ps { 2 } else { channels }).map_err(|_| "AAC channels overflow")?,
        )
        .ok_or("zero AAC channels")?,
        frame_size: NonZeroU32::new(core_samples * (output_rate / core_rate)),
        bit_depth: None,
        timing: AudioTiming::default(),
    })
}

fn read(bits: &mut BitReader<'_>, count: u32) -> Result<u32, Box<str>> {
    u32::try_from(
        bits.read_bits(count)
            .map_err(|_| "truncated AAC configuration")?,
    )
    .map_err(|_| "AAC field overflow".into())
}

fn frequency(bits: &mut BitReader<'_>) -> Result<u32, Box<str>> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let index = read(bits, 4)?;
    let rate = if index == 15 {
        read(bits, 24)?
    } else {
        *RATES
            .get(index as usize)
            .ok_or("reserved AAC sample rate")?
    };
    if rate == 0 {
        return Err("zero AAC sample rate".into());
    }
    Ok(rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use broadcast_common::bits::BitWriter;

    fn config(
        object: u64,
        core_rate: u64,
        channels: u64,
        extension_rate: Option<u64>,
        sync: bool,
    ) -> Vec<u8> {
        let mut data = [0u8; 16];
        let mut bits = BitWriter::new(&mut data);
        for (value, width) in [(object, 5), (core_rate, 4), (channels, 4)] {
            bits.write_bits(value, width).unwrap();
        }
        if matches!(object, 5 | 29) {
            bits.write_bits(extension_rate.unwrap(), 4).unwrap();
            bits.write_bits(2, 5).unwrap();
        }
        bits.write_bits(0, 3).unwrap();
        if sync {
            for (value, width) in [
                (0x2b7, 11),
                (5, 5),
                (1, 1),
                (extension_rate.unwrap(), 4),
                (0x548, 11),
                (1, 1),
            ] {
                bits.write_bits(value, width).unwrap();
            }
        }
        let length = bits.bits_written().div_ceil(8);
        data[..length].to_vec()
    }

    #[test]
    fn lc_he_and_ps_use_decoded_output_cadence() -> Result<(), Box<str>> {
        for (data, rate, channels, frame) in [
            (vec![0x11, 0x90], 48_000, 2, 1024),
            (vec![0x11, 0x94], 48_000, 2, 960),
            (config(5, 6, 2, Some(3), false), 48_000, 2, 2048),
            (config(29, 6, 1, Some(3), false), 48_000, 2, 2048),
            (config(2, 6, 1, Some(3), true), 48_000, 2, 2048),
            (config(5, 3, 2, Some(3), false), 48_000, 2, 1024),
        ] {
            let MediaParameters::Audio {
                sample_rate,
                channels: count,
                frame_size,
                ..
            } = parameters(&data)?
            else {
                panic!("audio")
            };
            assert_eq!(
                (sample_rate.get(), count.get(), frame_size.unwrap().get()),
                (rate, channels, frame)
            );
        }
        assert!(parameters(&[0x2b]).is_err());
        Ok(())
    }
}
