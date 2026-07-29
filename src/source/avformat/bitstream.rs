use std::{
    collections::VecDeque,
    ffi::{CStr, c_void},
    ptr::{self, NonNull},
};

use ffmpeg_sys_next as ffmpeg;

use crate::{
    ffmpeg::{AvError, OwnedPacket, replace_extradata},
    source::{DiscoveryProblem, SourceError},
};

use super::control::Control;

unsafe extern "C" {
    fn rushls_aac_adtstoasc_alloc(
        parameters: *const ffmpeg::AVCodecParameters,
        time_base: ffmpeg::AVRational,
        error: *mut i32,
    ) -> *mut c_void;
    fn rushls_bitstream_send(context: *mut c_void, packet: *mut ffmpeg::AVPacket) -> i32;
    fn rushls_bitstream_receive(context: *mut c_void, packet: *mut ffmpeg::AVPacket) -> i32;
    fn rushls_bitstream_free(context: *mut *mut c_void);
}

struct BitstreamFilter(NonNull<c_void>);

impl BitstreamFilter {
    /// # Safety
    ///
    /// The parameters and timebase must belong to one live AVFormat stream.
    unsafe fn aac_adtstoasc(
        parameters: *const ffmpeg::AVCodecParameters,
        time_base: ffmpeg::AVRational,
    ) -> Result<Self, SourceError> {
        let mut error = 0;
        let context = unsafe { rushls_aac_adtstoasc_alloc(parameters, time_base, &mut error) };
        NonNull::new(context).map(Self).ok_or_else(|| {
            SourceError::Demux(
                format!(
                    "initializing MPEG-TS AAC adaptation: {}",
                    AvError::new(error)
                )
                .into(),
            )
        })
    }

    fn send(&mut self, packet: *mut ffmpeg::AVPacket) -> i32 {
        unsafe { rushls_bitstream_send(self.0.as_ptr(), packet) }
    }

    fn receive(&mut self, packet: *mut ffmpeg::AVPacket) -> i32 {
        unsafe { rushls_bitstream_receive(self.0.as_ptr(), packet) }
    }
}

impl Drop for BitstreamFilter {
    fn drop(&mut self) {
        let mut context = self.0.as_ptr();
        unsafe { rushls_bitstream_free(&mut context) };
    }
}

/// Packet adaptation required by MPEG-TS but not by other AVFormat inputs.
///
/// The FIFO exists only during discovery and holds packets read while waiting
/// for AAC's in-band configuration. Once drained, steady-state filtering uses
/// the caller's reusable AVPacket directly.
pub struct MpegTsBitstreams {
    filters: Vec<Option<BitstreamFilter>>,
    prefetched: VecDeque<OwnedPacket>,
    draining: Option<usize>,
    demux_end: Option<i32>,
    flush_cursor: usize,
}

impl MpegTsBitstreams {
    /// Builds adapters only for an MPEG-TS input containing ordinary AAC.
    ///
    /// # Safety
    ///
    /// `context` must be a live, exclusively borrowed input context.
    pub unsafe fn for_input(
        context: *mut ffmpeg::AVFormatContext,
    ) -> Result<Option<Self>, SourceError> {
        if !unsafe { is_mpegts(context) } {
            return Ok(None);
        }
        let format = unsafe { &*context };
        let stream_count =
            usize::try_from(format.nb_streams).map_err(|_| DiscoveryProblem::OutOfRange {
                field: "stream count",
            })?;
        if stream_count > 0 && format.streams.is_null() {
            return Err(DiscoveryProblem::Missing {
                field: "stream table",
            }
            .into());
        }

        let mut has_aac = false;
        for index in 0..stream_count {
            let stream = unsafe { *format.streams.add(index) };
            if stream.is_null() || unsafe { (*stream).codecpar.is_null() } {
                continue;
            }
            let codec = unsafe { (*(*stream).codecpar).codec_id };
            reject_latm(codec)?;
            has_aac |= codec == ffmpeg::AVCodecID::AV_CODEC_ID_AAC;
        }
        if !has_aac {
            return Ok(None);
        }

        let mut filters = Vec::with_capacity(stream_count);
        for index in 0..stream_count {
            let stream = unsafe { *format.streams.add(index) };
            let filter = if !stream.is_null()
                && !unsafe { (*stream).codecpar.is_null() }
                && unsafe { (*(*stream).codecpar).codec_id } == ffmpeg::AVCodecID::AV_CODEC_ID_AAC
            {
                Some(unsafe {
                    BitstreamFilter::aac_adtstoasc((*stream).codecpar, (*stream).time_base)
                }?)
            } else {
                None
            };
            filters.push(filter);
        }
        Ok(Some(Self {
            filters,
            prefetched: VecDeque::new(),
            draining: None,
            demux_end: None,
            flush_cursor: 0,
        }))
    }

    /// Reads just far enough to promote in-band AAC configuration into
    /// codecpar before the domain track catalog is frozen.
    ///
    /// # Safety
    ///
    /// `context` must be the context used to construct these adapters.
    pub unsafe fn prime(
        &mut self,
        context: *mut ffmpeg::AVFormatContext,
        control: &Control,
    ) -> Result<(), SourceError> {
        let mut missing = Vec::with_capacity(self.filters.len());
        let mut remaining = 0_usize;
        for (index, filter) in self.filters.iter().enumerate() {
            let needs_configuration = filter.is_some()
                && unsafe {
                    let stream = *(*context).streams.add(index);
                    !stream.is_null()
                        && !(*stream).codecpar.is_null()
                        && (*(*stream).codecpar).extradata_size == 0
                };
            missing.push(needs_configuration);
            remaining += usize::from(needs_configuration);
        }
        if remaining == 0 {
            return Ok(());
        }

        let mut packet = OwnedPacket::new()
            .ok_or_else(|| SourceError::Open("could not allocate an AVPacket".into()))?;
        while remaining > 0 {
            let result = unsafe { self.read_filtered(context, packet.as_ptr()) };
            if result < 0 {
                if control.probe_exceeded() || control.interrupted() {
                    return Err(discovery_read_error(result, control));
                }
                if result == ffmpeg::AVERROR_EOF {
                    return Err(DiscoveryProblem::Missing {
                        field: "MPEG-TS AAC codec configuration",
                    }
                    .into());
                }
                return Err(discovery_read_error(result, control));
            }

            let stream_index = unsafe { (*packet.as_ptr()).stream_index };
            if let Ok(index) = usize::try_from(stream_index)
                && missing.get(index).copied() == Some(true)
                && let Some(configuration) = unsafe { new_extradata(packet.as_ptr()) }
            {
                let stream = unsafe { *(*context).streams.add(index) };
                unsafe { replace_extradata((*stream).codecpar, &configuration) }.map_err(
                    |error| {
                        SourceError::Demux(
                            format!("promoting MPEG-TS AAC configuration: {error}").into(),
                        )
                    },
                )?;
                missing[index] = false;
                remaining -= 1;
            }

            let retained = packet.try_clone().ok_or_else(|| {
                SourceError::Demux("could not retain a packet consumed during discovery".into())
            })?;
            self.prefetched.push_back(retained);
            packet.unref();
        }
        Ok(())
    }

    /// Returns a prefetched packet or reads the next adapted packet.
    ///
    /// # Safety
    ///
    /// `context` must be the context used to construct these adapters and
    /// `packet` must be a live, uniquely borrowed AVPacket.
    pub unsafe fn read(
        &mut self,
        context: *mut ffmpeg::AVFormatContext,
        packet: *mut ffmpeg::AVPacket,
    ) -> i32 {
        if let Some(prefetched) = self.prefetched.pop_front() {
            unsafe { ffmpeg::av_packet_unref(packet) };
            unsafe { ffmpeg::av_packet_move_ref(packet, prefetched.as_ptr()) };
            return 0;
        }
        unsafe { self.read_filtered(context, packet) }
    }

    unsafe fn read_filtered(
        &mut self,
        context: *mut ffmpeg::AVFormatContext,
        packet: *mut ffmpeg::AVPacket,
    ) -> i32 {
        loop {
            if let Some(index) = self.draining {
                let result = self.filters[index]
                    .as_mut()
                    .expect("only configured filters are drained")
                    .receive(packet);
                if result >= 0 {
                    return result;
                }
                self.draining = None;
                if result != ffmpeg::AVERROR(ffmpeg::EAGAIN) && result != ffmpeg::AVERROR_EOF {
                    return result;
                }
            }

            if let Some(end) = self.demux_end {
                while self.flush_cursor < self.filters.len() {
                    let index = self.flush_cursor;
                    self.flush_cursor += 1;
                    let Some(filter) = self.filters[index].as_mut() else {
                        continue;
                    };
                    let result = filter.send(ptr::null_mut());
                    if result < 0 && result != ffmpeg::AVERROR_EOF {
                        return result;
                    }
                    self.draining = Some(index);
                    break;
                }
                if self.draining.is_some() {
                    continue;
                }
                return end;
            }

            let result = unsafe { ffmpeg::av_read_frame(context, packet) };
            if result < 0 {
                self.demux_end = Some(result);
                continue;
            }
            let Ok(index) = usize::try_from(unsafe { (*packet).stream_index }) else {
                return result;
            };
            let Some(Some(filter)) = self.filters.get_mut(index) else {
                return result;
            };
            let result = filter.send(packet);
            if result < 0 {
                return result;
            }
            self.draining = Some(index);
        }
    }
}

unsafe fn is_mpegts(context: *mut ffmpeg::AVFormatContext) -> bool {
    if context.is_null() {
        return false;
    }
    let input = unsafe { (*context).iformat };
    if input.is_null() || unsafe { (*input).name.is_null() } {
        return false;
    }
    unsafe { CStr::from_ptr((*input).name) }
        .to_bytes()
        .split(|byte| *byte == b',')
        .any(|name| name == b"mpegts")
}

unsafe fn new_extradata(packet: *const ffmpeg::AVPacket) -> Option<Vec<u8>> {
    let mut size = 0_usize;
    let data = unsafe {
        ffmpeg::av_packet_get_side_data(
            packet,
            ffmpeg::AVPacketSideDataType::AV_PKT_DATA_NEW_EXTRADATA,
            &mut size,
        )
    };
    if data.is_null() || size == 0 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(data, size) }.to_vec())
}

fn reject_latm(codec: ffmpeg::AVCodecID) -> Result<(), SourceError> {
    if codec == ffmpeg::AVCodecID::AV_CODEC_ID_AAC_LATM {
        Err(SourceError::Demux(
            "MPEG-TS AAC-LATM is not supported; ADTS AAC is required".into(),
        ))
    } else {
        Ok(())
    }
}

fn discovery_read_error(result: i32, control: &Control) -> SourceError {
    if control.probe_exceeded() {
        DiscoveryProblem::ProbeLimitExceeded.into()
    } else if control.interrupted() {
        DiscoveryProblem::DeadlineExceeded.into()
    } else if let Some(error) = control.take_input_error() {
        SourceError::Input(error)
    } else {
        SourceError::Demux(
            format!(
                "adapting MPEG-TS packets during discovery: {}",
                AvError::new(result)
            )
            .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_latm_is_rejected_before_an_adapter_is_allocated() {
        let error = reject_latm(ffmpeg::AVCodecID::AV_CODEC_ID_AAC_LATM)
            .expect_err("AAC-LATM has no pass-through CMAF adaptation");
        assert!(error.to_string().contains("AAC-LATM"));
        assert!(reject_latm(ffmpeg::AVCodecID::AV_CODEC_ID_AAC).is_ok());
    }
}
