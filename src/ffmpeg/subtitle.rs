//! Subtitle packet side-data conversion at the FFmpeg boundary.

use std::{slice, str, sync::Arc};

use ffmpeg_sys_next as ffmpeg;

use crate::domain::{SubtitlePosition, WebVttCueMetadata};

/// Reads cue-level WebVTT text metadata.
///
/// # Safety
///
/// `packet` must point to a live initialized packet for this call.
pub unsafe fn read_webvtt_metadata(
    packet: *const ffmpeg::AVPacket,
    maximum_bytes: usize,
) -> Result<WebVttCueMetadata, Box<str>> {
    // Bound each copy by the packet's remaining budget. At most one bounded
    // allocation can precede rejection of an oversized combined record.
    let identifier = unsafe {
        read_text_side_data(
            packet,
            ffmpeg::AVPacketSideDataType::AV_PKT_DATA_WEBVTT_IDENTIFIER,
            "WebVTT cue identifier",
            maximum_bytes,
        )
    }?;
    let remaining_bytes = maximum_bytes.saturating_sub(identifier.as_deref().map_or(0, str::len));
    let settings = unsafe {
        read_text_side_data(
            packet,
            ffmpeg::AVPacketSideDataType::AV_PKT_DATA_WEBVTT_SETTINGS,
            "WebVTT cue settings",
            remaining_bytes,
        )
    }?;

    Ok(WebVttCueMetadata {
        identifier,
        settings,
    })
}

/// Reads SubRip's optional pixel-space cue rectangle.
///
/// # Safety
///
/// `packet` must point to a live initialized packet for this call.
pub unsafe fn read_subtitle_position(
    packet: *const ffmpeg::AVPacket,
) -> Result<Option<SubtitlePosition>, Box<str>> {
    const POSITION_BYTES: usize = 16;

    let mut size = 0_usize;
    // SAFETY: guaranteed by the caller; FFmpeg owns the borrowed data.
    let data = unsafe {
        ffmpeg::av_packet_get_side_data(
            packet,
            ffmpeg::AVPacketSideDataType::AV_PKT_DATA_SUBTITLE_POSITION,
            &mut size,
        )
    };
    if data.is_null() {
        return Ok(None);
    }
    if size != POSITION_BYTES {
        return Err(format!(
            "subtitle-position side data is {size} bytes, expected {POSITION_BYTES}"
        )
        .into());
    }
    // SAFETY: FFmpeg reported the complete sixteen-byte coordinate record.
    let bytes = unsafe { slice::from_raw_parts(data, POSITION_BYTES) };
    let coordinate = |offset: usize| {
        i32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("coordinate is four bytes"),
        )
    };
    Ok(Some(SubtitlePosition {
        x1: coordinate(0),
        y1: coordinate(4),
        x2: coordinate(8),
        y2: coordinate(12),
    }))
}

unsafe fn read_text_side_data(
    packet: *const ffmpeg::AVPacket,
    kind: ffmpeg::AVPacketSideDataType,
    name: &'static str,
    maximum_bytes: usize,
) -> Result<Option<Arc<str>>, Box<str>> {
    let mut size = 0_usize;
    // SAFETY: guaranteed by the caller; FFmpeg owns the borrowed data.
    let data = unsafe { ffmpeg::av_packet_get_side_data(packet, kind, &mut size) };
    if data.is_null() {
        return Ok(None);
    }
    if size > maximum_bytes {
        return Err(format!(
            "WebVTT cue metadata is {size} bytes, above the {maximum_bytes}-byte limit"
        )
        .into());
    }
    // SAFETY: FFmpeg reported `size` readable bytes.
    let bytes = unsafe { slice::from_raw_parts(data, size) };
    let value =
        str::from_utf8(bytes).map_err(|_| format!("{name} is not valid UTF-8").into_boxed_str())?;
    if value.contains('\0') {
        return Err(format!("{name} contains a NUL byte").into());
    }
    Ok(Some(Arc::from(value)))
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::ffmpeg::OwnedPacket;

    #[test]
    fn webvtt_text_side_data_is_copied_into_semantic_metadata() -> Result<(), Box<str>> {
        let packet = OwnedPacket::new().ok_or("packet allocation succeeds")?;
        for (kind, value) in [
            (
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_WEBVTT_IDENTIFIER,
                b"cue-7".as_slice(),
            ),
            (
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_WEBVTT_SETTINGS,
                b"align:start".as_slice(),
            ),
        ] {
            // SAFETY: the test exclusively owns this live packet.
            let data =
                unsafe { ffmpeg::av_packet_new_side_data(packet.as_ptr(), kind, value.len()) };
            assert!(!data.is_null());
            // SAFETY: FFmpeg allocated exactly `value.len()` writable bytes.
            unsafe { ptr::copy_nonoverlapping(value.as_ptr(), data, value.len()) };
        }

        // SAFETY: the packet remains live for the read.
        let metadata = unsafe { read_webvtt_metadata(packet.as_ptr(), usize::MAX) }?;
        assert_eq!(metadata.identifier.as_deref(), Some("cue-7"));
        assert_eq!(metadata.settings.as_deref(), Some("align:start"));
        Ok(())
    }

    #[test]
    fn webvtt_metadata_is_bounded_before_it_is_retained() -> Result<(), Box<str>> {
        let packet = OwnedPacket::new().ok_or("packet allocation succeeds")?;
        // SAFETY: the test exclusively owns this live packet.
        let data = unsafe {
            ffmpeg::av_packet_new_side_data(
                packet.as_ptr(),
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_WEBVTT_IDENTIFIER,
                5,
            )
        };
        assert!(!data.is_null());
        // SAFETY: FFmpeg allocated exactly five writable bytes.
        unsafe { ptr::copy_nonoverlapping(b"cue-7".as_ptr(), data, 5) };

        // SAFETY: the packet remains live for the read.
        let error = unsafe { read_webvtt_metadata(packet.as_ptr(), 4) }
            .expect_err("metadata above the packet limit is rejected");
        assert!(error.contains("above the 4-byte limit"));
        Ok(())
    }

    #[test]
    fn subtitle_position_requires_the_exact_ffmpeg_layout() -> Result<(), Box<str>> {
        let packet = OwnedPacket::new().ok_or("packet allocation succeeds")?;
        // SAFETY: the test exclusively owns this live packet.
        let data = unsafe {
            ffmpeg::av_packet_new_side_data(
                packet.as_ptr(),
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_SUBTITLE_POSITION,
                15,
            )
        };
        assert!(!data.is_null());

        // SAFETY: the packet remains live for the read.
        assert!(
            unsafe { read_subtitle_position(packet.as_ptr()) }
                .expect_err("short position is malformed")
                .contains("expected 16")
        );
        Ok(())
    }
}
