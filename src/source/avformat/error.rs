use std::ffi::CStr;

use ffmpeg_sys_next as ffmpeg;

#[derive(Clone, Debug, Eq, PartialEq, derive_more::Display)]
#[display("{message} ({code})")]
pub struct AvError {
    code: i32,
    message: String,
}

impl AvError {
    pub fn new(code: i32) -> Self {
        let mut buffer = [0_i8; 128];
        // SAFETY: `buffer` is valid for its full length and FFmpeg always
        // writes a terminating NUL when the call succeeds.
        let result = unsafe { ffmpeg::av_strerror(code, buffer.as_mut_ptr(), buffer.len()) };
        let message = if result >= 0 {
            // SAFETY: successful `av_strerror` initialized a NUL-terminated
            // string inside `buffer`.
            unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            "unknown FFmpeg error".to_string()
        };
        Self { code, message }
    }
}
