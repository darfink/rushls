use std::{ffi::CStr, ptr};

use ffmpeg_sys_next as ffmpeg;

use super::AvError;

/// Owned option dictionary passed through FFmpeg's consuming `**options` APIs.
pub struct Dictionary(*mut ffmpeg::AVDictionary);

impl Dictionary {
    pub fn new() -> Self {
        Self(ptr::null_mut())
    }

    pub fn set(&mut self, key: &CStr, value: &CStr) -> Result<(), AvError> {
        // SAFETY: FFmpeg copies both NUL-terminated strings into this owned
        // dictionary.
        let result = unsafe { ffmpeg::av_dict_set(&mut self.0, key.as_ptr(), value.as_ptr(), 0) };
        if result < 0 {
            return Err(AvError::new(result));
        }
        Ok(())
    }

    pub fn as_mut_ptr(&mut self) -> *mut *mut ffmpeg::AVDictionary {
        &mut self.0
    }

    pub fn first_key(&self) -> Option<String> {
        if self.0.is_null() {
            return None;
        }
        // SAFETY: an empty key plus AV_DICT_IGNORE_SUFFIX returns the first
        // entry from this live dictionary.
        let entry = unsafe {
            ffmpeg::av_dict_get(
                self.0,
                c"".as_ptr(),
                ptr::null(),
                ffmpeg::AV_DICT_IGNORE_SUFFIX,
            )
        };
        if entry.is_null() {
            return None;
        }
        // SAFETY: dictionary entries remain live while `self` does.
        Some(
            unsafe { CStr::from_ptr((*entry).key) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

impl Drop for Dictionary {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the dictionary.
        unsafe { ffmpeg::av_dict_free(&mut self.0) };
    }
}

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
    fn owns_options_and_copies_exact_values() {
        let mut dictionary = Dictionary::new();
        dictionary
            .set(c"title", c"Camera")
            .expect("dictionary accepts strings");

        // SAFETY: the wrapper keeps its dictionary live.
        assert_eq!(
            unsafe { value(*dictionary.as_mut_ptr(), c"title") },
            Some("Camera".into())
        );
        assert_eq!(dictionary.first_key(), Some("title".into()));
    }
}
