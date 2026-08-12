use std::{
    ffi::c_void,
    ptr::{self, NonNull},
    slice,
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use ffmpeg_sys_next as ffmpeg;

use crate::{
    domain::{Codec, Payload, TrackId, WebVttCueMetadata},
    ffmpeg::{AvError, OwnedPacket, read_audio_trim, read_subtitle_position, read_webvtt_metadata},
    source::{DiscoveryLimits, DiscoveryProblem, InputState, Packet, SourceError},
};

use super::{
    bitstream::MpegTsBitstreams,
    control::Control,
    input::{AvformatInput, AvformatInputError},
    metadata::StreamCatalog,
};

struct ReadOpaque {
    input: Box<dyn AvformatInput>,
    control: Arc<Control>,
}

/// The `va_list` parameter FFmpeg's log callback prototype uses.
///
/// `va_list` is already a pointer typedef on Apple targets, so the typedef is
/// the parameter type. On SysV x86 it is an array typedef
/// (`__va_list_tag[1]`), and a function parameter of array type decays to a
/// pointer, so bindgen binds the prototype's parameter as that pointer; a
/// callback written with the array typedef itself no longer matches.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
type FfmpegLogVaList = *mut ffmpeg::__va_list_tag;
#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86"))))]
type FfmpegLogVaList = ffmpeg::va_list;

/// Silences one benign FFmpeg message without hiding anything else.
///
/// FLV script-data captions log "OnTextData packet is not implemented" once per
/// cue even though flvdec extracts the cue text successfully, so a captioned
/// stream would emit a line per cue for the life of the session. FFmpeg offers
/// no per-context threshold for a demuxer — `log_level_offset` exists on
/// `AVCodecContext`, not `AVFormatContext` — so the message is matched on its
/// format string and dropped, and everything else reaches the default handler
/// unchanged.
///
/// Installed once per process, on the first demuxer opened.
fn install_log_filter() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: FFmpeg stores the pointer and calls it for later logging;
        // the callback below is a plain `extern "C"` function with no state.
        unsafe { ffmpeg::av_log_set_callback(Some(filter_log)) };
    });
}

/// The subject `avpriv_request_sample` is called with for a caption cue.
const ONTEXTDATA_SUBJECT: &str = "OnTextData packet";

/// The two sentences FFmpeg appends to every `avpriv_request_sample` notice.
///
/// Emitted as separate `av_log` calls that name no subject of their own, so
/// they can only be attributed to whichever notice preceded them.
const REQUEST_SAMPLE_TAIL: [&str; 2] = [
    " is not implemented. Update your FFmpeg ",
    "If you want to help, upload a sample ",
];

thread_local! {
    /// Whether the notice currently being emitted is the one being dropped.
    ///
    /// `avpriv_request_sample` logs its subject and then its two fixed
    /// sentences in immediate succession on the calling thread, so tracking
    /// the subject is what lets the tail be dropped for *our* notice only. A
    /// different unimplemented feature keeps its whole message, rather than
    /// printing a subject whose explanation was swallowed.
    static DROPPING_NOTICE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "C" fn filter_log(
    class: *mut c_void,
    level: libc::c_int,
    format: *const libc::c_char,
    arguments: FfmpegLogVaList,
) {
    if !format.is_null() {
        // `avpriv_request_sample` passes its subject through the format string,
        // so matching here needs no argument formatting and cannot be confused
        // by a publisher's own text.
        // SAFETY: FFmpeg format strings are NUL-terminated literals.
        let text = unsafe { std::ffi::CStr::from_ptr(format) }.to_bytes();
        if starts_with(text, ONTEXTDATA_SUBJECT.as_bytes()) {
            DROPPING_NOTICE.with(|dropping| dropping.set(true));
            return;
        }
        let tail = REQUEST_SAMPLE_TAIL
            .iter()
            .position(|sentence| starts_with(text, sentence.as_bytes()));
        let dropping = DROPPING_NOTICE.with(std::cell::Cell::get);
        match tail {
            Some(index) if dropping => {
                // The invitation is the last line of the notice.
                if index + 1 == REQUEST_SAMPLE_TAIL.len() {
                    DROPPING_NOTICE.with(|dropping| dropping.set(false));
                }
                return;
            }
            // Any other message ends the notice being tracked, so a later
            // unrelated tail cannot be attributed to it.
            None => DROPPING_NOTICE.with(|dropping| dropping.set(false)),
            Some(_) => {}
        }
    }
    // SAFETY: the arguments are exactly those FFmpeg passed in.
    unsafe { ffmpeg::av_log_default_callback(class, level, format, arguments) };
}

fn starts_with(text: &[u8], prefix: &[u8]) -> bool {
    text.len() >= prefix.len() && &text[..prefix.len()] == prefix
}

struct Avio {
    context: NonNull<ffmpeg::AVIOContext>,
    _opaque: Box<ReadOpaque>,
}

impl Avio {
    fn new(
        input: Box<dyn AvformatInput>,
        control: Arc<Control>,
        buffer_size: usize,
    ) -> Result<Self, SourceError> {
        let buffer_size = i32::try_from(buffer_size)
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| SourceError::Open("invalid AVIO buffer size".into()))?;
        let buffer_len = usize::try_from(buffer_size)
            .map_err(|_| SourceError::Open("AVIO buffer size exceeds address space".into()))?;
        // SAFETY: FFmpeg owns this allocation after `avio_alloc_context`
        // succeeds. The failure path frees it below.
        let buffer = unsafe { ffmpeg::av_malloc(buffer_len) }.cast::<u8>();
        let Some(buffer) = NonNull::new(buffer) else {
            return Err(SourceError::Open(
                "could not allocate the AVIO buffer".into(),
            ));
        };

        let mut opaque = Box::new(ReadOpaque { input, control });
        // SAFETY: every pointer is valid and remains alive in `Self`; this is a
        // read-only, non-seekable byte stream.
        let context = unsafe {
            ffmpeg::avio_alloc_context(
                buffer.as_ptr(),
                buffer_size,
                0,
                (&raw mut *opaque).cast(),
                Some(read_packet),
                None,
                None,
            )
        };
        let Some(context) = NonNull::new(context) else {
            // SAFETY: ownership was not transferred because allocation failed.
            unsafe { ffmpeg::av_free(buffer.as_ptr().cast()) };
            return Err(SourceError::Open(
                "could not allocate the AVIO context".into(),
            ));
        };
        Ok(Self {
            context,
            _opaque: opaque,
        })
    }

    fn as_ptr(&self) -> *mut ffmpeg::AVIOContext {
        self.context.as_ptr()
    }
}

impl Drop for Avio {
    fn drop(&mut self) {
        let mut context = self.context.as_ptr();
        // SAFETY: this object uniquely owns the AVIO context. FFmpeg also
        // releases the possibly replaced internal buffer here.
        unsafe { ffmpeg::avio_context_free(&raw mut context) };
    }
}

unsafe extern "C" fn read_packet(opaque: *mut c_void, buffer: *mut u8, buffer_size: i32) -> i32 {
    if opaque.is_null() || buffer.is_null() || buffer_size <= 0 {
        return ffmpeg::AVERROR(ffmpeg::EINVAL);
    }
    // SAFETY: `Avio` keeps this `ReadOpaque` alive for the callback's lifetime.
    let opaque = unsafe { &mut *opaque.cast::<ReadOpaque>() };
    if opaque.control.interrupted() {
        return ffmpeg::AVERROR_EXIT;
    }
    let Ok(buffer_size) = usize::try_from(buffer_size) else {
        return ffmpeg::AVERROR(ffmpeg::EINVAL);
    };
    let buffer_size = opaque.control.limit_read(buffer_size);
    if buffer_size == 0 {
        return ffmpeg::AVERROR_EXIT;
    }
    // SAFETY: FFmpeg supplied a writable buffer of `buffer_size` bytes.
    let buffer = unsafe { slice::from_raw_parts_mut(buffer, buffer_size) };
    match opaque.input.read(buffer, opaque.control.as_ref()) {
        Ok(read) if read <= buffer_size => {
            opaque.control.record_read(read);
            i32::try_from(read).unwrap_or(i32::MAX)
        }
        Ok(read) => {
            opaque.control.set_input_error(
                format!("byte input reported {read} bytes for a {buffer_size}-byte buffer").into(),
            );
            ffmpeg::AVERROR(ffmpeg::EIO)
        }
        Err(AvformatInputError::End(state)) => {
            opaque.control.set_terminal(match state {
                InputState::Open => InputState::Interrupted,
                state => state,
            });
            ffmpeg::AVERROR_EOF
        }
        Err(AvformatInputError::Failed(error)) => {
            opaque.control.set_input_error(error);
            ffmpeg::AVERROR(ffmpeg::EIO)
        }
    }
}

unsafe extern "C" fn interrupt(opaque: *mut c_void) -> i32 {
    if opaque.is_null() {
        return 1;
    }
    // SAFETY: `Avio` owns the `Arc<Control>` whose pointee remains stable.
    i32::from(unsafe { &*opaque.cast::<Control>() }.interrupted())
}

pub struct FormatInput {
    context: NonNull<ffmpeg::AVFormatContext>,
    _io: Avio,
    control: Arc<Control>,
    mpegts: Option<MpegTsBitstreams>,
}

impl FormatInput {
    pub fn open(
        input: Box<dyn AvformatInput>,
        control: Arc<Control>,
        limits: DiscoveryLimits,
        io_buffer_size: usize,
    ) -> Result<(Self, StreamCatalog), SourceError> {
        if limits.maximum_probe_bytes == 0 {
            return Err(DiscoveryProblem::LimitNotPositive {
                field: "maximum probe bytes",
            }
            .into());
        }
        install_log_filter();
        control.set_deadline(Some(Instant::now() + limits.maximum_wall_time));
        control.begin_probe(limits.maximum_probe_bytes);
        let io = Avio::new(input, Arc::clone(&control), io_buffer_size)?;
        // SAFETY: no arguments and no ownership prerequisites.
        let context = unsafe { ffmpeg::avformat_alloc_context() };
        let Some(context) = NonNull::new(context) else {
            return Err(SourceError::Open(
                "could not allocate the AVFormat context".into(),
            ));
        };
        // SAFETY: the context is uniquely owned until `avformat_open_input`.
        unsafe {
            (*context.as_ptr()).pb = io.as_ptr();
            (*context.as_ptr()).flags |= ffmpeg::AVFMT_FLAG_CUSTOM_IO;
            (*context.as_ptr()).probesize =
                i64::try_from(limits.maximum_probe_bytes).unwrap_or(i64::MAX);
            (*context.as_ptr()).interrupt_callback = ffmpeg::AVIOInterruptCB {
                callback: Some(interrupt),
                opaque: Arc::as_ptr(&control).cast_mut().cast(),
            };
        }

        let mut opened = context.as_ptr();
        // SAFETY: `opened` points to a configured input context. Custom IO
        // means neither a URL nor an explicit input format is required.
        let result = unsafe {
            ffmpeg::avformat_open_input(&raw mut opened, ptr::null(), ptr::null(), ptr::null_mut())
        };
        if result < 0 {
            if !opened.is_null() {
                // SAFETY: FFmpeg left a live context in `opened`. This is the
                // one failure the type below cannot cover, because ownership
                // has not come back to us yet.
                unsafe { ffmpeg::avformat_close_input(&raw mut opened) };
            }
            return Err(open_error("opening input", result, &control));
        }
        let context = NonNull::new(opened)
            .ok_or_else(|| SourceError::Open("AVFormat returned a null context".into()))?;

        // Take ownership the moment FFmpeg hands the context back, before doing
        // anything else that can fail. Everything below is now covered by
        // `Drop`, so a later `?` releases the context instead of leaking a whole
        // demuxer per failed session — which is the path a hostile input takes.
        let format = Self {
            context,
            _io: io,
            control,
            mpegts: None,
        };

        // SAFETY: the context is open and exclusively owned by `format`.
        let result =
            unsafe { ffmpeg::avformat_find_stream_info(format.context(), ptr::null_mut()) };
        if result < 0 {
            return Err(discovery_error(result, &format.control));
        }
        // FFmpeg may return success after its interrupt callback stopped a
        // probe. The context is not usable in that case: the byte limit can
        // have landed inside a container packet, leaving the demuxer poised in
        // the middle of that packet when normal reads resume.
        if format.control.probe_exceeded() {
            return Err(DiscoveryProblem::ProbeLimitExceeded.into());
        }
        if format.control.interrupted() {
            return Err(DiscoveryProblem::DeadlineExceeded.into());
        }
        let mut format = format;
        // MPEG-TS carries AAC configuration in-band. Adapt and retain only the
        // packets needed to expose it before freezing the public track catalog.
        let mut mpegts = unsafe { MpegTsBitstreams::for_input(format.context()) }?;
        if let Some(bitstreams) = &mut mpegts {
            unsafe { bitstreams.prime(format.context(), &format.control) }?;
        }
        format.mpegts = mpegts;
        format.control.set_deadline(None);
        format.control.finish_probe();
        // SAFETY: stream discovery has completed on this open context.
        let catalog = unsafe { StreamCatalog::discover(format.context()) }?;
        Ok((format, catalog))
    }

    pub fn context(&self) -> *mut ffmpeg::AVFormatContext {
        self.context.as_ptr()
    }

    pub fn read(&mut self, packet: &mut AvPacket) -> i32 {
        // SAFETY: both objects are live and uniquely borrowed. Non-MPEG-TS
        // inputs retain the direct AVFormat path with no adapter allocation.
        match &mut self.mpegts {
            Some(bitstreams) => unsafe { bitstreams.read(self.context.as_ptr(), packet.as_ptr()) },
            None => unsafe { ffmpeg::av_read_frame(self.context.as_ptr(), packet.as_ptr()) },
        }
    }

    pub fn read_error(&self, code: i32) -> ReadError {
        if self.control.cancelled() {
            return ReadError::Cancelled;
        }
        if let Some(error) = self.control.take_input_error() {
            return ReadError::Failed(SourceError::Input(error));
        }
        if code == ffmpeg::AVERROR_EOF {
            return ReadError::End(self.control.terminal().unwrap_or(InputState::Interrupted));
        }
        if code == ffmpeg::AVERROR_EXIT {
            return ReadError::End(InputState::Interrupted);
        }
        ReadError::Failed(SourceError::Input(AvError::new(code).to_string().into()))
    }
}

impl Drop for FormatInput {
    fn drop(&mut self) {
        let mut context = self.context.as_ptr();
        // SAFETY: this object uniquely owns the open format context.
        unsafe { ffmpeg::avformat_close_input(&raw mut context) };
    }
}

pub enum ReadError {
    End(InputState),
    Failed(SourceError),
    Cancelled,
}

pub struct AvPacket(OwnedPacket);

impl AvPacket {
    pub fn new() -> Result<Self, SourceError> {
        OwnedPacket::new()
            .map(Self)
            .ok_or_else(|| SourceError::Open("could not allocate an AVPacket".into()))
    }

    pub fn as_ptr(&mut self) -> *mut ffmpeg::AVPacket {
        self.0.as_ptr()
    }

    pub fn to_packet(
        &self,
        track_id: TrackId,
        codec: Codec,
        maximum_payload_bytes: usize,
    ) -> Result<Packet, SourceError> {
        // SAFETY: the packet is live and initialized by `av_read_frame`.
        let packet = unsafe { &*self.0.as_ptr() };
        let size = usize::try_from(packet.size)
            .map_err(|_| SourceError::Input("AVPacket has a negative payload size".into()))?;
        if size > maximum_payload_bytes {
            return Err(SourceError::PacketPayloadTooLarge {
                limit: maximum_payload_bytes,
                found: size,
            });
        }
        if size > 0 && packet.data.is_null() {
            return Err(SourceError::Input(
                "AVPacket has a null payload pointer".into(),
            ));
        }
        let payload = if size == 0 {
            Payload::default()
        } else if packet.buf.is_null() {
            // Unusual non-reference-counted packets cannot outlive `unref`;
            // copying is the only safe fallback.
            // SAFETY: AVPacket guarantees `size` readable payload bytes.
            Payload::from(unsafe { slice::from_raw_parts(packet.data, size) }.to_vec())
        } else {
            // SAFETY: the source packet owns a live AVBuffer reference.
            let buffer = unsafe { ffmpeg::av_buffer_ref(packet.buf) };
            let buffer = NonNull::new(buffer)
                .ok_or_else(|| SourceError::Input("could not retain AVPacket payload".into()))?;
            Payload::from_bytes(Bytes::from_owner(PacketPayload {
                buffer,
                data: packet.data,
                size,
            }))
        };
        // SAFETY: the packet remains live through this conversion.
        let webvtt = if codec == Codec::WebVtt {
            unsafe {
                read_webvtt_metadata(self.0.as_ptr(), maximum_payload_bytes.saturating_sub(size))
            }
            .map_err(SourceError::Input)?
        } else {
            WebVttCueMetadata::default()
        };
        // SAFETY: the packet remains live through this conversion.
        let subtitle_position = if codec == Codec::SubRip {
            unsafe { read_subtitle_position(self.0.as_ptr()) }.map_err(SourceError::Input)?
        } else {
            None
        };
        let retained = size
            .checked_add(webvtt.retained_bytes())
            .ok_or_else(|| SourceError::Input("packet payload accounting overflowed".into()))?;
        if retained > maximum_payload_bytes {
            return Err(SourceError::PacketPayloadTooLarge {
                limit: maximum_payload_bytes,
                found: retained,
            });
        }
        Ok(Packet {
            track_id,
            pts: timestamp(packet.pts),
            dts: timestamp(packet.dts),
            duration: (packet.duration > 0).then_some(packet.duration),
            random_access: packet.flags & ffmpeg::AV_PKT_FLAG_KEY != 0,
            // SAFETY: the packet remains live through this conversion.
            audio_trim: unsafe { read_audio_trim(self.0.as_ptr()) }
                .map_err(SourceError::Input)?
                .unwrap_or_default(),
            webvtt,
            subtitle_position,
            payload,
        })
    }

    pub fn unref(&mut self) {
        self.0.unref();
    }
}

struct PacketPayload {
    buffer: NonNull<ffmpeg::AVBufferRef>,
    data: *const u8,
    size: usize,
}

// SAFETY: `av_buffer_ref` created an independent reference to an immutable
// AVBuffer. FFmpeg buffer references may be released from a thread other than
// the one that created them.
unsafe impl Send for PacketPayload {}

impl AsRef<[u8]> for PacketPayload {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the owned AVPacket keeps this exact data region alive.
        unsafe { slice::from_raw_parts(self.data, self.size) }
    }
}

impl Drop for PacketPayload {
    fn drop(&mut self) {
        let mut buffer = self.buffer.as_ptr();
        // SAFETY: this owner uniquely owns its AVBuffer reference.
        unsafe { ffmpeg::av_buffer_unref(&raw mut buffer) };
    }
}

fn timestamp(value: i64) -> Option<i64> {
    (value != ffmpeg::AV_NOPTS_VALUE).then_some(value)
}

fn open_error(action: &str, code: i32, control: &Control) -> SourceError {
    if control.probe_exceeded() {
        DiscoveryProblem::ProbeLimitExceeded.into()
    } else if control.interrupted() {
        DiscoveryProblem::DeadlineExceeded.into()
    } else if let Some(error) = control.take_input_error() {
        SourceError::Open(error)
    } else {
        SourceError::Open(format!("{action}: {}", AvError::new(code)).into())
    }
}

fn discovery_error(code: i32, control: &Control) -> SourceError {
    if control.probe_exceeded() {
        DiscoveryProblem::ProbeLimitExceeded.into()
    } else if control.interrupted() {
        DiscoveryProblem::DeadlineExceeded.into()
    } else if let Some(error) = control.take_input_error() {
        SourceError::Input(error)
    } else {
        SourceError::Demux(AvError::new(code).to_string().into())
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::Arc, time::Duration};

    use super::*;
    use crate::source::avformat::{ReadInput, fixtures};

    #[test]
    fn non_mpeg_ts_inputs_keep_the_direct_read_path() {
        let control = Arc::new(Control::new());
        let (format, _) = FormatInput::open(
            Box::new(ReadInput::closed(Cursor::new(fixtures::primed_aac_mkv()))),
            control,
            DiscoveryLimits {
                maximum_probe_bytes: 1024 * 1024,
                maximum_wall_time: Duration::from_secs(2),
            },
            32 * 1024,
        )
        .expect("Matroska fixture opens");

        assert!(
            format.mpegts.is_none(),
            "non-MPEG-TS inputs allocate neither a filter nor a prefetch FIFO"
        );
    }
}
