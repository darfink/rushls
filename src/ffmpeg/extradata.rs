use std::{ffi::c_void, ptr::NonNull};

use ffmpeg_sys_next as ffmpeg;

/// Replaces codec configuration with an FFmpeg-owned, padded copy.
///
/// # Safety
///
/// `parameters` must point to live, uniquely borrowed codec parameters.
pub unsafe fn replace_extradata(
    parameters: *mut ffmpeg::AVCodecParameters,
    extradata: &[u8],
) -> Result<(), Box<str>> {
    if parameters.is_null() {
        return Err("codec parameters are null".into());
    }
    let size = i32::try_from(extradata.len())
        .map_err(|_| Box::<str>::from("codec extradata exceeds FFmpeg range"))?;
    let allocation = extradata
        .len()
        .checked_add(ffmpeg::AV_INPUT_BUFFER_PADDING_SIZE as usize)
        .ok_or_else(|| Box::<str>::from("codec extradata size overflowed"))?;
    // FFmpeg permits `av_freep` on a null field and clears the pointer itself.
    unsafe {
        ffmpeg::av_freep((&raw mut (*parameters).extradata).cast::<c_void>());
        (*parameters).extradata_size = 0;
    }
    if extradata.is_empty() {
        return Ok(());
    }
    // Zeroed padding is required because optimized bitstream readers may read
    // beyond the logical end of codec configuration.
    let data = unsafe { ffmpeg::av_mallocz(allocation) }.cast::<u8>();
    let Some(data) = NonNull::new(data) else {
        return Err("could not allocate codec extradata".into());
    };
    unsafe {
        std::ptr::copy_nonoverlapping(extradata.as_ptr(), data.as_ptr(), extradata.len());
        (*parameters).extradata = data.as_ptr();
        (*parameters).extradata_size = size;
    }
    Ok(())
}
