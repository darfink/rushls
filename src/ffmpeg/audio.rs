//! Audio timing conversion at the FFmpeg packet boundary.

use std::{ptr, slice};

use ffmpeg_sys_next as ffmpeg;

use crate::domain::AudioTrim;

const SKIP_SAMPLES_BYTES: usize = 10;

/// Reads FFmpeg's skip-samples side data into the domain's semantic form.
///
/// # Safety
///
/// `packet` must point to a live initialized packet for this call.
pub unsafe fn read_audio_trim(
    packet: *const ffmpeg::AVPacket,
) -> Result<Option<AudioTrim>, Box<str>> {
    let mut size = 0_usize;
    // SAFETY: guaranteed by the caller; FFmpeg owns the returned borrowed data.
    let data = unsafe {
        ffmpeg::av_packet_get_side_data(
            packet,
            ffmpeg::AVPacketSideDataType::AV_PKT_DATA_SKIP_SAMPLES,
            &mut size,
        )
    };
    if data.is_null() {
        return Ok(None);
    }
    if size < SKIP_SAMPLES_BYTES {
        return Err(format!(
            "skip-samples side data is {size} bytes, expected at least {SKIP_SAMPLES_BYTES}"
        )
        .into());
    }
    // SAFETY: FFmpeg reported at least the ten bytes defined by packet.h.
    let bytes = unsafe { slice::from_raw_parts(data, SKIP_SAMPLES_BYTES) };
    let trim = AudioTrim {
        leading_samples: u32::from_le_bytes(
            bytes[..4].try_into().expect("leading count is four bytes"),
        ),
        trailing_samples: u32::from_le_bytes(
            bytes[4..8]
                .try_into()
                .expect("trailing count is four bytes"),
        ),
    };
    Ok((!trim.is_empty()).then_some(trim))
}

/// Attaches semantic audio trimming using FFmpeg's private side-data layout.
///
/// Reason bytes are deliberately zero. They are advisory, FFmpeg's MOV muxer
/// consumes only the sample counts, and no FFmpeg-specific reason vocabulary
/// belongs in the public media contract.
///
/// # Safety
///
/// `packet` must point to a live initialized packet exclusively owned by the
/// caller.
pub unsafe fn write_audio_trim(
    packet: *mut ffmpeg::AVPacket,
    trim: AudioTrim,
) -> Result<(), Box<str>> {
    if trim.is_empty() {
        return Ok(());
    }
    // SAFETY: guaranteed by the caller; FFmpeg owns the new allocation.
    let data = unsafe {
        ffmpeg::av_packet_new_side_data(
            packet,
            ffmpeg::AVPacketSideDataType::AV_PKT_DATA_SKIP_SAMPLES,
            SKIP_SAMPLES_BYTES,
        )
    };
    if data.is_null() {
        return Err("could not allocate skip-samples side data".into());
    }
    // SAFETY: FFmpeg allocated exactly `SKIP_SAMPLES_BYTES` writable bytes.
    unsafe {
        ptr::copy_nonoverlapping(trim.leading_samples.to_le_bytes().as_ptr(), data, 4);
        ptr::copy_nonoverlapping(trim.trailing_samples.to_le_bytes().as_ptr(), data.add(4), 4);
        *data.add(8) = 0;
        *data.add(9) = 0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::OwnedPacket;

    #[test]
    fn skip_sample_side_data_round_trips_leading_and_trailing_trim() {
        let packet = OwnedPacket::new().expect("packet allocation succeeds");
        let trim = AudioTrim {
            leading_samples: 1_024,
            trailing_samples: 127,
        };

        // SAFETY: the test exclusively owns this live packet.
        unsafe { write_audio_trim(packet.as_ptr(), trim) }.expect("side data attaches");
        // SAFETY: the packet remains live for the read.
        assert_eq!(unsafe { read_audio_trim(packet.as_ptr()) }, Ok(Some(trim)));
    }

    #[test]
    fn malformed_skip_sample_side_data_is_rejected() {
        let packet = OwnedPacket::new().expect("packet allocation succeeds");
        // SAFETY: the test exclusively owns this live packet.
        let data = unsafe {
            ffmpeg::av_packet_new_side_data(
                packet.as_ptr(),
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_SKIP_SAMPLES,
                SKIP_SAMPLES_BYTES - 1,
            )
        };
        assert!(!data.is_null(), "side-data allocation succeeds");

        // SAFETY: the packet remains live for the read.
        let error =
            unsafe { read_audio_trim(packet.as_ptr()) }.expect_err("short side data is malformed");
        assert!(error.contains("expected at least 10"));
    }
}
