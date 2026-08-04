use std::ptr::NonNull;

use ffmpeg_sys_next as ffmpeg;

/// Reusable owned `AVPacket` allocation shared by ingest and muxing.
pub struct OwnedPacket(NonNull<ffmpeg::AVPacket>);

impl OwnedPacket {
    pub fn new() -> Option<Self> {
        // SAFETY: no arguments or ownership prerequisites.
        NonNull::new(unsafe { ffmpeg::av_packet_alloc() }).map(Self)
    }

    pub fn as_ptr(&self) -> *mut ffmpeg::AVPacket {
        self.0.as_ptr()
    }

    pub fn unref(&mut self) {
        // SAFETY: this wrapper uniquely owns the packet reference.
        unsafe { ffmpeg::av_packet_unref(self.0.as_ptr()) };
    }

    pub fn try_clone(&self) -> Option<Self> {
        // SAFETY: the source packet remains live for this call. FFmpeg either
        // retains its reference-counted buffers or copies unowned storage.
        NonNull::new(unsafe { ffmpeg::av_packet_clone(self.0.as_ptr()) }).map(Self)
    }
}

// SAFETY: an AVPacket allocation has no thread affinity. The wrapper owns it
// uniquely and exposes mutation only through an exclusive borrow or FFmpeg
// calls made by its exclusive owner.
unsafe impl Send for OwnedPacket {}

impl Drop for OwnedPacket {
    fn drop(&mut self) {
        let mut packet = self.0.as_ptr();
        // SAFETY: this wrapper uniquely owns the allocation.
        unsafe { ffmpeg::av_packet_free(&raw mut packet) };
    }
}
