//! FLAC framing and timing (RFC 9639). No PCM decoding or timestamp rebasing.

use crate::domain::{AudioTiming, MediaParameters};
use std::{
    num::{NonZeroU16, NonZeroU32},
    ops::Range,
};

#[derive(Clone, Copy, Debug)]
pub struct StreamInfo {
    pub rate: u32,
    pub channels: u16,
    pub depth: u16,
    pub maximum_block: u32,
}

/// Require an entire native FLAC metadata header, as carried by Enhanced RTMP.
/// Keep only STREAMINFO in MP4: seek tables and file checksums describe the input
/// file, not the fragmented output or its possible missing intervals.
pub fn configuration(data: &[u8]) -> Result<(StreamInfo, transmux::FlacSpecificBox), Box<str>> {
    if data.get(..4) != Some(b"fLaC") {
        return Err("FLAC metadata requires the fLaC marker".into());
    }
    let mut offset = 4;
    let mut streaminfo = None;
    loop {
        let header = data
            .get(offset..offset + 4)
            .ok_or("truncated FLAC metadata header")?;
        let kind = header[0] & 127;
        let last = header[0] & 128 != 0;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        offset += 4;
        let block = data
            .get(offset..offset + len)
            .ok_or("truncated FLAC metadata block")?;
        if kind == 127 || (offset == 8 && kind != 0) {
            return Err("invalid FLAC metadata block type".into());
        }
        if kind == 0 {
            if streaminfo.is_some() || len != 34 {
                return Err("invalid FLAC STREAMINFO".into());
            }
            streaminfo = Some(block);
        }
        offset += len;
        if last {
            break;
        }
    }
    if offset != data.len() {
        return Err("trailing FLAC configuration bytes".into());
    }
    let block = streaminfo.ok_or("missing FLAC STREAMINFO")?;
    let min = u32::from(u16::from_be_bytes([block[0], block[1]]));
    let max = u32::from(u16::from_be_bytes([block[2], block[3]]));
    let packed = u64::from_be_bytes(block[10..18].try_into().expect("STREAMINFO field"));
    let info = StreamInfo {
        rate: (packed >> 44) as u32,
        channels: ((packed >> 41) & 7) as u16 + 1,
        depth: ((packed >> 36) & 31) as u16 + 1,
        maximum_block: max,
    };
    // AudioSampleEntry uses a 16.16 sample rate. Start with mono/stereo, where
    // channel order needs no additional channel-layout signaling.
    if info.rate == 0
        || info.rate > 65_535
        || info.channels > 2
        || info.depth < 4
        || min < 16
        || max < min
    {
        return Err(
            "unsupported FLAC STREAMINFO (requires mono/stereo, 1–65535 Hz, valid block sizes)"
                .into(),
        );
    }
    let mut output = block.to_vec();
    // Total samples and MD5 are unknown for a live fragmented presentation.
    output[13] &= 0xf0;
    output[14..34].fill(0);
    Ok((
        info,
        transmux::FlacSpecificBox {
            version: 0,
            flags: 0,
            blocks: vec![transmux::FlacMetadataBlock {
                last: true,
                block_type: 0,
                data: output,
            }],
        },
    ))
}

pub fn parameters(data: &[u8]) -> Result<MediaParameters, Box<str>> {
    let (info, _) = configuration(data)?;
    Ok(MediaParameters::Audio {
        sample_rate: NonZeroU32::new(info.rate).ok_or("zero FLAC rate")?,
        channels: NonZeroU16::new(info.channels).ok_or("zero FLAC channels")?,
        bit_depth: NonZeroU16::new(info.depth),
        // Even fixed-block FLAC permits a short final frame. Every packet's
        // decoded duration comes from its own header, never STREAMINFO's maximum.
        frame_size: None,
        timing: AudioTiming::default(),
    })
}

#[derive(Debug)]
pub struct Frame {
    pub range: Range<usize>,
    pub samples: u32,
}

/// Parse subframe lengths, rather than scanning for sync patterns inside coded
/// data. Both CRCs must pass before any part of the message is emitted.
pub fn frames(
    data: &[u8],
    info: StreamInfo,
    maximum_frames: usize,
) -> Result<Vec<Frame>, Box<str>> {
    let mut bits = Bits { data, position: 0 };
    let mut frames = Vec::new();
    let mut expected_number = None;
    while bits.position < data.len() * 8 {
        if frames.len() >= maximum_frames {
            return Err("too many FLAC frames in one message".into());
        }
        let start = bits.position / 8;
        let Header {
            samples,
            channels,
            depth,
            count,
            number,
            variable,
        } = header(&mut bits, info)?;
        if let Some((expected, strategy)) = expected_number
            && (number != expected || variable != strategy)
        {
            return Err("noncontiguous FLAC frames within one RTMP message".into());
        }
        expected_number = Some((
            number + if variable { u64::from(samples) } else { 1 },
            variable,
        ));
        for channel in 0..count {
            let extra = (channels == 8 && channel == 1)
                || (channels == 9 && channel == 0)
                || (channels == 10 && channel == 1);
            subframe(&mut bits, samples, u32::from(depth) + u32::from(extra))?;
        }
        let padding = (8 - bits.position % 8) % 8;
        if bits.read(u32::try_from(padding).expect("less than eight bits"))? != 0 {
            return Err("nonzero FLAC frame padding".into());
        }
        bits.read(16)?;
        let end = bits.position / 8;
        if crc(&data[start..end], 16, 0x8005) != 0 {
            return Err("FLAC frame CRC mismatch".into());
        }
        frames.push(Frame {
            range: start..end,
            samples,
        });
    }
    if frames.is_empty() {
        return Err("empty FLAC coded message".into());
    }
    Ok(frames)
}

struct Header {
    samples: u32,
    channels: u32,
    depth: u16,
    count: u32,
    number: u64,
    variable: bool,
}

fn header(bits: &mut Bits<'_>, info: StreamInfo) -> Result<Header, Box<str>> {
    let start = bits.position / 8;
    if bits.read(14)? != 0x3ffe || bits.read(1)? != 0 {
        return Err("invalid FLAC frame sync".into());
    }
    let variable = bits.read(1)? != 0;
    let block = bits.read(4)?;
    let rate_code = bits.read(4)?;
    let channels = bits.read(4)?;
    let depth_code = bits.read(3)?;
    if bits.read(1)? != 0 {
        return Err("reserved FLAC frame bit".into());
    }
    let number = coded_number(bits, variable)?;
    let samples = match block {
        1 => 192,
        2..=5 => 576 << (block - 2),
        6 => bits.read(8)? + 1,
        7 => bits.read(16)? + 1,
        8..=15 => 256 << (block - 8),
        _ => return Err("reserved FLAC block size".into()),
    };
    let rate = match rate_code {
        0 => info.rate,
        1 => 88200,
        2 => 176_400,
        3 => 192_000,
        4 => 8000,
        5 => 16000,
        6 => 22050,
        7 => 24000,
        8 => 32000,
        9 => 44100,
        10 => 48000,
        11 => 96000,
        12 => bits.read(8)? * 1000,
        13 => bits.read(16)?,
        14 => bits.read(16)? * 10,
        _ => return Err("reserved FLAC sample rate".into()),
    };
    let depth = match depth_code {
        0 => info.depth,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return Err("reserved FLAC bit depth".into()),
    };
    let count = match channels {
        0..=7 => channels + 1,
        8..=10 => 2,
        _ => return Err("reserved FLAC channel assignment".into()),
    };
    if rate != info.rate
        || depth != info.depth
        || count != u32::from(info.channels)
        || samples > info.maximum_block
    {
        return Err("FLAC frame disagrees with STREAMINFO".into());
    }
    bits.read(8)?;
    if crc(&bits.data[start..bits.position / 8], 8, 0x07) != 0 {
        return Err("FLAC header CRC mismatch".into());
    }
    Ok(Header {
        samples,
        channels,
        depth,
        count,
        number,
        variable,
    })
}

fn coded_number(bits: &mut Bits<'_>, variable: bool) -> Result<u64, Box<str>> {
    let first = u8::try_from(bits.read(8)?).expect("eight bits");
    let leading = first.leading_ones();
    let mut number = if leading == 0 {
        u64::from(first)
    } else {
        if !(2..=7).contains(&leading) {
            return Err("invalid FLAC coded number".into());
        }
        u64::from(first & (0x7f >> leading))
    };
    for _ in 1..leading {
        let byte = bits.read(8)?;
        if byte & 0xc0 != 0x80 {
            return Err("invalid FLAC coded number continuation".into());
        }
        number = (number << 6) | u64::from(byte & 63);
    }
    let minimum = match leading {
        0 => 0,
        2 => 128,
        3 => 2048,
        4 => 65536,
        5 => 2_097_152,
        6 => 67_108_864,
        7 => 2_147_483_648,
        _ => unreachable!(),
    };
    if number < minimum || number >= (1u64 << if variable { 36 } else { 31 }) {
        return Err("invalid FLAC coded number range".into());
    }
    Ok(number)
}

fn subframe(bits: &mut Bits<'_>, samples: u32, mut depth: u32) -> Result<(), Box<str>> {
    if bits.read(1)? != 0 {
        return Err("reserved FLAC subframe bit".into());
    }
    let kind = bits.read(6)?;
    if bits.read(1)? != 0 {
        let wasted = bits.unary()? + 1;
        depth = depth
            .checked_sub(wasted)
            .filter(|n| *n > 0)
            .ok_or("invalid FLAC wasted bits")?;
    }
    let order = match kind {
        0 => {
            return bits.skip(u64::from(depth));
        }
        1 => {
            return bits.skip(u64::from(depth) * u64::from(samples));
        }
        8..=12 => kind - 8,
        32..=63 => kind - 31,
        _ => return Err("reserved FLAC subframe type".into()),
    };
    if order > samples {
        return Err("FLAC predictor exceeds block size".into());
    }
    bits.skip(u64::from(order) * u64::from(depth))?;
    if kind >= 32 {
        let precision = bits.read(4)? + 1;
        if precision == 16 {
            return Err("reserved FLAC predictor precision".into());
        }
        let shift = bits.read(5)?;
        if shift & 16 != 0 {
            return Err("negative FLAC predictor shift".into());
        }
        bits.skip(u64::from(precision) * u64::from(order))?;
    }
    let method = bits.read(2)?;
    if method > 1 {
        return Err("reserved FLAC residual method".into());
    }
    let partitions = 1u32 << bits.read(4)?;
    if !samples.is_multiple_of(partitions) || samples / partitions <= order {
        return Err("invalid FLAC residual partitions".into());
    }
    for partition in 0..partitions {
        let count = samples / partitions - if partition == 0 { order } else { 0 };
        let width = 4 + method;
        let rice = bits.read(width)?;
        if rice == (1 << width) - 1 {
            let raw = bits.read(5)?;
            bits.skip(u64::from(count) * u64::from(raw))?;
        } else {
            for _ in 0..count {
                bits.unary()?;
                bits.skip(u64::from(rice))?;
            }
        }
    }
    Ok(())
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: u32) -> Result<u32, Box<str>> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self
                .data
                .get(self.position / 8)
                .ok_or("truncated FLAC frame")?;
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }
    fn skip(&mut self, count: u64) -> Result<(), Box<str>> {
        let count = usize::try_from(count).map_err(|_| "FLAC bit count overflows")?;
        self.position = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.data.len() * 8)
            .ok_or("truncated FLAC subframe")?;
        Ok(())
    }
    fn unary(&mut self) -> Result<u32, Box<str>> {
        let mut zeros = 0u32;
        while self.read(1)? == 0 {
            zeros = zeros.checked_add(1).ok_or("FLAC unary value overflows")?;
        }
        Ok(zeros)
    }
}

fn crc(data: &[u8], width: u32, polynomial: u32) -> u32 {
    let mut value = 0;
    for byte in data {
        value ^= u32::from(*byte) << (width - 8);
        for _ in 0..8 {
            value = (value << 1)
                ^ if value & (1 << (width - 1)) != 0 {
                    polynomial
                } else {
                    0
                };
        }
        value &= (1 << width) - 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::fixtures::{FLAC_MONO, FLAC_STEREO, flac_parts};

    #[test]
    fn real_frames_have_exact_durations_and_valid_checksums() -> Result<(), Box<str>> {
        for (bytes, rate, channels, depth) in
            [(FLAC_MONO, 44100, 1, 16), (FLAC_STEREO, 48000, 2, 24)]
        {
            let (header, coded) = flac_parts(bytes);
            let (info, config) = configuration(header)?;
            assert_eq!(
                (info.rate, info.channels, info.depth),
                (rate, channels, depth)
            );
            let parsed = frames(coded, info, 4096)?;
            assert_eq!(
                parsed.iter().map(|f| f.samples).sum::<u32>(),
                rate * 71 / 100
            );
            assert!(parsed.last().expect("tail").samples < 1024);
            assert_eq!(parsed.first().expect("first").range.start, 0);
            assert_eq!(parsed.last().expect("last").range.end, coded.len());
            assert_eq!(config.blocks.len(), 1);
            assert_eq!(&config.blocks[0].data[18..], &[0; 16]);
            assert!(frames(coded, info, 1).is_err());
            let mut corrupt = coded.to_vec();
            corrupt[10] ^= 1;
            assert!(frames(&corrupt, info, 4096).is_err());
            for end in 0..parsed[0].range.end {
                assert!(frames(&coded[..end], info, 4096).is_err());
            }
            for end in 0..header.len() {
                assert!(configuration(&header[..end]).is_err());
            }
            let mut changed = info;
            changed.rate += 1;
            assert!(frames(coded, changed, 4096).is_err());
            let mut missing = coded[..parsed[0].range.end].to_vec();
            missing.extend_from_slice(&coded[parsed[2].range.start..]);
            assert!(frames(&missing, info, 4096).is_err());
        }
        Ok(())
    }
    fn constant_frame(variable: bool, number: u8, samples: u8) -> Vec<u8> {
        let mut frame = vec![
            0xff,
            if variable { 0xf9 } else { 0xf8 },
            0x69,
            0x08,
            number,
            samples - 1,
        ];
        frame.push(u8::try_from(crc(&frame, 8, 7)).expect("CRC-8"));
        frame.extend_from_slice(&[0, 0, 0]);
        let footer = u16::try_from(crc(&frame, 16, 0x8005)).expect("CRC-16");
        frame.extend_from_slice(&footer.to_be_bytes());
        frame
    }

    #[test]
    fn variable_blocks_use_sample_numbers_and_exact_durations() -> Result<(), Box<str>> {
        let info = StreamInfo {
            rate: 44100,
            channels: 1,
            depth: 16,
            maximum_block: 32,
        };
        let first = constant_frame(true, 0, 16);
        let mut coded = first.clone();
        coded.extend(constant_frame(true, 16, 32));
        let parsed = frames(&coded, info, 2)?;
        assert_eq!(
            parsed.iter().map(|f| f.samples).collect::<Vec<_>>(),
            vec![16, 32]
        );
        for bad in [constant_frame(true, 17, 32), constant_frame(false, 1, 32)] {
            let mut coded = first.clone();
            coded.extend(bad);
            assert!(frames(&coded, info, 2).is_err());
        }
        // A valid CRC does not excuse a reserved header field.
        let mut bad = first;
        bad[3] |= 1;
        bad[6] = u8::try_from(crc(&bad[..6], 8, 7)).expect("CRC-8");
        assert!(frames(&bad, info, 2).is_err());
        Ok(())
    }

    #[test]
    fn metadata_rejects_duplicate_truncated_and_unsupported_streaminfo() -> Result<(), Box<str>> {
        let (header, _) = flac_parts(FLAC_MONO);
        let mut duplicate = header[..42].to_vec();
        duplicate[4] = 0;
        duplicate.extend_from_slice(&header[4..42]);
        duplicate[42] |= 128;
        assert!(configuration(&duplicate).is_err());
        let mut truncated = header.to_vec();
        truncated[5..8].copy_from_slice(&[255; 3]);
        assert!(configuration(&truncated).is_err());
        let mut surround = header.to_vec();
        surround[20] |= 0x0e;
        assert!(configuration(&surround).is_err());
        let (_, config) = configuration(header)?;
        assert_eq!(config.blocks[0].data[13] & 15, 0);
        assert!(config.blocks[0].data[14..].iter().all(|b| *b == 0));
        Ok(())
    }
}
