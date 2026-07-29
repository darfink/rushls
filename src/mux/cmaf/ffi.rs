use std::{
    ffi::c_void,
    ptr::{self, NonNull},
    slice,
};

use bytes::BytesMut;
use ffmpeg_sys_next as av;

use crate::{
    domain::{Codec, DiscoveredTrack, MediaParameters, Payload},
    ffmpeg::{
        AvError, Dictionary, OwnedPacket, from_av_rational, replace_extradata, to_av_rational,
        write_audio_trim,
    },
    media::NormalizedSample,
};

struct WriteBuffer {
    bytes: BytesMut,
}

struct OutputAvio {
    context: NonNull<av::AVIOContext>,
    opaque: Box<WriteBuffer>,
}

impl OutputAvio {
    fn new(buffer_size: usize) -> Result<Self, Box<str>> {
        let buffer_size = i32::try_from(buffer_size)
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| Box::<str>::from("invalid output AVIO buffer size"))?;
        // SAFETY: FFmpeg owns this allocation after `avio_alloc_context`
        // succeeds. The failure path releases it explicitly.
        let buffer = unsafe { av::av_malloc(buffer_size as usize) }.cast::<u8>();
        let Some(buffer) = NonNull::new(buffer) else {
            return Err("could not allocate output AVIO buffer".into());
        };
        let mut opaque = Box::new(WriteBuffer {
            bytes: BytesMut::new(),
        });
        // SAFETY: all pointers remain owned by `Self`; this context is
        // write-only and deliberately non-seekable.
        let context = unsafe {
            av::avio_alloc_context(
                buffer.as_ptr(),
                buffer_size,
                1,
                (&mut *opaque as *mut WriteBuffer).cast(),
                None,
                Some(write_packet),
                None,
            )
        };
        let Some(context) = NonNull::new(context) else {
            // SAFETY: allocation ownership was not transferred on failure.
            unsafe { av::av_free(buffer.as_ptr().cast()) };
            return Err("could not allocate output AVIO context".into());
        };
        // SAFETY: this context is uniquely owned and has no seek callback.
        unsafe { (*context.as_ptr()).seekable = 0 };
        Ok(Self { context, opaque })
    }

    fn as_ptr(&self) -> *mut av::AVIOContext {
        self.context.as_ptr()
    }

    fn take(&mut self) -> Result<Payload, Box<str>> {
        // SAFETY: the live context is uniquely borrowed and flushing invokes
        // the callback synchronously.
        unsafe { av::avio_flush(self.context.as_ptr()) };
        Ok(Payload::from_bytes(self.opaque.bytes.split().freeze()))
    }
}

impl Drop for OutputAvio {
    fn drop(&mut self) {
        let mut context = self.context.as_ptr();
        // SAFETY: this object uniquely owns the AVIO context and its internal
        // allocation.
        unsafe { av::avio_context_free(&mut context) };
    }
}

unsafe extern "C" fn write_packet(opaque: *mut c_void, buffer: *const u8, buffer_size: i32) -> i32 {
    if opaque.is_null() || buffer.is_null() || buffer_size < 0 {
        return av::AVERROR(av::EINVAL);
    }
    let Ok(size) = usize::try_from(buffer_size) else {
        return av::AVERROR(av::EINVAL);
    };
    // SAFETY: `OutputAvio` keeps the opaque value alive for every callback.
    let output = unsafe { &mut *opaque.cast::<WriteBuffer>() };
    // SAFETY: FFmpeg supplied exactly `size` readable bytes for this callback.
    output
        .bytes
        .extend_from_slice(unsafe { slice::from_raw_parts(buffer, size) });
    buffer_size
}

pub(super) struct FormatOutput {
    context: NonNull<av::AVFormatContext>,
    packet: OwnedPacket,
    io: OutputAvio,
    finalized: bool,
}

// SAFETY: every operation requires exclusive access, and FFmpeg's per-context
// muxing state has no thread affinity. Moving this owner between threads does
// not permit concurrent access to any of its pointers.
unsafe impl Send for FormatOutput {}

impl FormatOutput {
    pub(super) fn open(track: &DiscoveredTrack, io_buffer_size: usize) -> Result<Self, Box<str>> {
        // Everything that does not need the format context is allocated first,
        // so the context can be wrapped the instant FFmpeg returns it. Every
        // failure below then unwinds through `Drop` rather than a hand-written
        // free that the next early return would silently skip.
        let io = OutputAvio::new(io_buffer_size)?;
        let packet = OwnedPacket::new()
            .ok_or_else(|| Box::<str>::from("could not allocate an output packet"))?;

        let mut context = ptr::null_mut();
        // SAFETY: FFmpeg allocates a new output context for the named muxer.
        let result = unsafe {
            av::avformat_alloc_output_context2(
                &mut context,
                ptr::null(),
                c"mp4".as_ptr(),
                ptr::null(),
            )
        };
        if result < 0 {
            return Err(format!("allocating MP4 output: {}", AvError::new(result)).into());
        }
        let context = NonNull::new(context)
            .ok_or_else(|| Box::<str>::from("FFmpeg returned a null output context"))?;
        let mut output = Self {
            context,
            packet,
            io,
            finalized: false,
        };

        // SAFETY: the context is live and uniquely owned by `output`.
        let stream = unsafe { av::avformat_new_stream(output.context.as_ptr(), ptr::null()) };
        let stream = NonNull::new(stream)
            .ok_or_else(|| Box::<str>::from("could not allocate an MP4 stream"))?;
        output.configure_stream(stream, track)?;
        // SAFETY: custom IO remains owned by `output`; the format context must
        // not attempt to open or close it.
        unsafe {
            (*output.context.as_ptr()).pb = output.io.as_ptr();
            (*output.context.as_ptr()).flags |= av::AVFMT_FLAG_CUSTOM_IO;
        }

        let mut options = Dictionary::new();
        options
            .set(c"movflags", c"cmaf+dash+skip_sidx+frag_custom+delay_moov")
            .map_err(|error| format!("setting CMAF flags: {error}").into_boxed_str())?;
        options
            .set(c"use_editlist", c"1")
            .map_err(|error| format!("enabling MP4 edit lists: {error}").into_boxed_str())?;
        // SAFETY: the output context has one fully configured stream and live
        // custom IO. FFmpeg consumes recognized dictionary entries.
        let result =
            unsafe { av::avformat_write_header(output.context.as_ptr(), options.as_mut_ptr()) };
        if result < 0 {
            return Err(format!("writing MP4 header: {}", AvError::new(result)).into());
        }
        if let Some(option) = options.first_key() {
            return Err(format!("FFmpeg did not consume output option {option}").into());
        }
        // SAFETY: the stream remains owned by the live output context.
        let negotiated = unsafe { from_av_rational((*stream.as_ptr()).time_base) }
            .map_err(|error| format!("invalid negotiated output timebase: {error}"))?;
        if negotiated != track.timebase {
            // The header was written, so some muxers hold internal state that
            // only the trailer releases. Discard whatever it produces.
            let _ = output.finalize();
            return Err(format!(
                "FFmpeg changed the timebase of {} from {:?} to {:?}",
                track.id, track.timebase, negotiated
            )
            .into());
        }
        Ok(output)
    }

    fn configure_stream(
        &mut self,
        stream: NonNull<av::AVStream>,
        track: &DiscoveredTrack,
    ) -> Result<(), Box<str>> {
        let timebase = to_av_rational(track.timebase)
            .map_err(|error| format!("unrepresentable track timebase: {error}"))?;
        // SAFETY: the stream and codec parameters are uniquely owned by this
        // not-yet-open output context.
        unsafe {
            (*stream.as_ptr()).id = 0;
            (*stream.as_ptr()).time_base = timebase;
            let parameters = (*stream.as_ptr()).codecpar;
            if parameters.is_null() {
                return Err("FFmpeg allocated a stream without codec parameters".into());
            }
            (*parameters).codec_type = media_type(track);
            (*parameters).codec_id = codec_id(track.codec)?;
            match track.parameters {
                MediaParameters::Video { width, height, .. } => {
                    (*parameters).width = i32::try_from(width.get())
                        .map_err(|_| Box::<str>::from("video width exceeds FFmpeg range"))?;
                    (*parameters).height = i32::try_from(height.get())
                        .map_err(|_| Box::<str>::from("video height exceeds FFmpeg range"))?;
                }
                MediaParameters::Audio {
                    sample_rate,
                    channels,
                    frame_size,
                    bit_depth,
                    timing,
                } => {
                    (*parameters).sample_rate = i32::try_from(sample_rate.get())
                        .map_err(|_| Box::<str>::from("audio sample rate exceeds FFmpeg range"))?;
                    (*parameters).frame_size = frame_size
                        .map(|value| i32::try_from(value.get()))
                        .transpose()
                        .map_err(|_| Box::<str>::from("audio frame size exceeds FFmpeg range"))?
                        .unwrap_or_default();
                    (*parameters).bits_per_raw_sample = bit_depth
                        .map(|value| i32::from(value.get()))
                        .unwrap_or_default();
                    (*parameters).initial_padding =
                        audio_timing_field(timing.initial_padding_samples, "initial padding")?;
                    (*parameters).trailing_padding =
                        audio_timing_field(timing.trailing_padding_samples, "trailing padding")?;
                    (*parameters).seek_preroll =
                        audio_timing_field(timing.seek_preroll_samples, "seek preroll")?;
                    av::av_channel_layout_default(
                        &mut (*parameters).ch_layout,
                        i32::from(channels.get()),
                    );
                }
                MediaParameters::Subtitle => {
                    return Err("subtitle tracks are not supported by the CMAF muxer".into());
                }
            }
            require_extradata(&track.codec_extradata)?;
            // SAFETY: these codec parameters belong exclusively to the
            // not-yet-open output stream.
            replace_extradata(parameters, track.codec_extradata.as_bytes())?;
        }
        Ok(())
    }

    pub(super) fn write(
        &mut self,
        sample: &NormalizedSample,
        pts: i64,
        dts: i64,
    ) -> Result<(), Box<str>> {
        let payload = sample_payload(sample);
        let size = i32::try_from(payload.len())
            .map_err(|_| Box::<str>::from("sample payload exceeds FFmpeg packet range"))?;
        // SAFETY: the reusable packet is unreferenced after every call.
        let result = unsafe { av::av_new_packet(self.packet.as_ptr(), size) };
        if result < 0 {
            return Err(format!("allocating output packet: {}", AvError::new(result)).into());
        }
        // SAFETY: `av_new_packet` allocated `size` writable bytes.
        unsafe {
            if !payload.is_empty() {
                ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    (*self.packet.as_ptr()).data,
                    payload.len(),
                );
            }
            (*self.packet.as_ptr()).stream_index = 0;
            (*self.packet.as_ptr()).pts = pts;
            (*self.packet.as_ptr()).dts = dts;
            (*self.packet.as_ptr()).duration = i64::try_from(sample.duration()).unwrap_or(i64::MAX);
            (*self.packet.as_ptr()).flags = if sample.random_access() {
                av::AV_PKT_FLAG_KEY
            } else {
                0
            };
        }
        if let NormalizedSample::Audio(audio) = sample {
            // SAFETY: the packet is initialized and exclusively owned here.
            if let Err(error) = unsafe { write_audio_trim(self.packet.as_ptr(), audio.trim) } {
                self.packet.unref();
                return Err(error);
            }
        }
        // The payload is copied into an FFmpeg-owned packet rather than wrapped
        // zero-copy. `av_write_frame` is synchronous and does not take
        // ownership, so a custom `AVBufferRef` over the domain's immutable
        // `Payload` would have to promise the mutable, `AV_INPUT_BUFFER_PADDING_
        // SIZE`-padded storage FFmpeg is entitled to assume — a promise
        // `Payload` deliberately cannot make.
        //
        // Measured before accepting it, since this is the hottest path in the
        // system: this whole function, copy and mov muxing together, runs at
        // 5.7 GB/s for 60 KB frames and 6.4 GB/s for 400 KB frames. A 50 Mb/s
        // 4K feed is 6.25 MB/s, so packaging one costs about a thousandth of a
        // core. The copy is not where this system will run out of headroom, and
        // removing it would trade that for unsafe buffer-lifetime FFI.
        let result = unsafe { av::av_write_frame(self.context.as_ptr(), self.packet.as_ptr()) };
        // SAFETY: this wrapper uniquely owns the reusable packet.
        self.packet.unref();
        if result < 0 {
            return Err(format!("writing MP4 packet: {}", AvError::new(result)).into());
        }
        Ok(())
    }

    pub(super) fn flush_fragment(&mut self) -> Result<Payload, Box<str>> {
        // SAFETY: a null packet is the documented `frag_custom` flush signal.
        let result = unsafe { av::av_write_frame(self.context.as_ptr(), ptr::null_mut()) };
        if result < 0 {
            return Err(format!("flushing MP4 fragment: {}", AvError::new(result)).into());
        }
        self.io.take()
    }

    pub(super) fn finalize(&mut self) -> Result<(), Box<str>> {
        if self.finalized {
            return Ok(());
        }
        self.finalized = true;
        // SAFETY: the header was written successfully and the context is live.
        let result = unsafe { av::av_write_trailer(self.context.as_ptr()) };
        // Trailer indexes are not CMAF media objects; drain and discard them.
        let _ = self.io.take()?;
        if result < 0 {
            return Err(format!("finalizing MP4 output: {}", AvError::new(result)).into());
        }
        Ok(())
    }
}

impl Drop for FormatOutput {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the format context. The packet
        // and custom AVIO fields release their separate allocations.
        unsafe { av::avformat_free_context(self.context.as_ptr()) };
    }
}

fn audio_timing_field(value: u32, field: &str) -> Result<i32, Box<str>> {
    i32::try_from(value).map_err(|_| format!("audio {field} exceeds FFmpeg range").into())
}

fn codec_id(codec: Codec) -> Result<av::AVCodecID, Box<str>> {
    match codec {
        Codec::H264 => Ok(av::AVCodecID::AV_CODEC_ID_H264),
        Codec::Hevc => Ok(av::AVCodecID::AV_CODEC_ID_HEVC),
        Codec::Av1 => Ok(av::AVCodecID::AV_CODEC_ID_AV1),
        Codec::Aac => Ok(av::AVCodecID::AV_CODEC_ID_AAC),
        Codec::Opus => Ok(av::AVCodecID::AV_CODEC_ID_OPUS),
        Codec::MovText | Codec::SubRip | Codec::WebVtt => {
            Err("subtitle codecs are not supported by the CMAF muxer".into())
        }
        Codec::Unknown(id) => Err(format!("codec {id} is not supported by the CMAF muxer").into()),
    }
}

fn media_type(track: &DiscoveredTrack) -> av::AVMediaType {
    match track.parameters {
        MediaParameters::Video { .. } => av::AVMediaType::AVMEDIA_TYPE_VIDEO,
        MediaParameters::Audio { .. } => av::AVMediaType::AVMEDIA_TYPE_AUDIO,
        MediaParameters::Subtitle => av::AVMediaType::AVMEDIA_TYPE_SUBTITLE,
    }
}

fn require_extradata(extradata: &Payload) -> Result<(), Box<str>> {
    if extradata.is_empty() {
        return Err("CMAF pass-through requires codec extradata".into());
    }
    Ok(())
}

fn sample_payload(sample: &NormalizedSample) -> &[u8] {
    match sample {
        NormalizedSample::Video(sample) => sample.payload.as_bytes(),
        NormalizedSample::Audio(sample) => sample.payload.as_bytes(),
        NormalizedSample::Subtitle(sample) => sample.payload.as_bytes(),
    }
}
