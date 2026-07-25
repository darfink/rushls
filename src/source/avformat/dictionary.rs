use std::{ffi::CStr, ptr};

use ffmpeg_sys_next as ffmpeg;

/// Copies one exact dictionary value while its FFmpeg owner is live.
///
/// # Safety
///
/// `dictionary` must be null or point to a live `AVDictionary`.
pub unsafe fn value(dictionary: *const ffmpeg::AVDictionary, key: &CStr) -> Option<String> {
    if dictionary.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller; exact matching requires no flags.
    let entry = unsafe { ffmpeg::av_dict_get(dictionary, key.as_ptr(), ptr::null(), 0) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: dictionary entries remain valid while the dictionary is live.
    let value = unsafe { (*entry).value };
    if value.is_null() {
        return None;
    }
    // SAFETY: FFmpeg dictionary values are NUL-terminated C strings.
    Some(
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_exact_metadata_values_out_of_ffmpeg() {
        let mut dictionary = ptr::null_mut();
        // SAFETY: FFmpeg allocates and owns the dictionary entry.
        let result = unsafe {
            ffmpeg::av_dict_set(&mut dictionary, c"title".as_ptr(), c"Camera".as_ptr(), 0)
        };
        assert_eq!(result, 0);

        // SAFETY: `dictionary` remains live until it is freed below.
        assert_eq!(
            unsafe { value(dictionary, c"title") },
            Some("Camera".into())
        );
        // SAFETY: `dictionary` remains live until it is freed below.
        assert_eq!(unsafe { value(dictionary, c"language") }, None);

        // SAFETY: this test uniquely owns the dictionary.
        unsafe { ffmpeg::av_dict_free(&mut dictionary) };
    }
}
