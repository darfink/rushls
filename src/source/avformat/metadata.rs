use std::{num::NonZero, slice};

use ffmpeg_sys_next as ffmpeg;

use crate::{
    domain::{
        Codec, DiscoveredTrack, FrameRate, MediaParameters, Payload, Timebase, TrackCatalog,
        TrackId,
    },
    source::{DiscoveryReport, SourceError},
};

use super::dictionary;

pub struct StreamCatalog {
    stream_count: usize,
    tracks: Vec<Option<TrackSnapshot>>,
    report: DiscoveryReport,
}

impl StreamCatalog {
    /// Extracts every stream field the application relies on and snapshots the
    /// codec configuration used to reject unsupported mid-stream changes.
    ///
    /// # Safety
    ///
    /// `context` must be an open input context exclusively owned by the caller.
    pub unsafe fn discover(context: *mut ffmpeg::AVFormatContext) -> Result<Self, SourceError> {
        // SAFETY: guaranteed by the caller.
        let format = unsafe { &*context };
        let stream_count = usize::try_from(format.nb_streams)
            .map_err(|_| SourceError::Discovery("stream count does not fit usize".into()))?;
        if stream_count > 0 && format.streams.is_null() {
            return Err(SourceError::Discovery(
                "AVFormat returned a null stream table".into(),
            ));
        }

        let mut snapshots = Vec::with_capacity(stream_count);
        let mut discovered = Vec::with_capacity(stream_count);
        for index in 0..stream_count {
            // SAFETY: `streams` has `nb_streams` entries by AVFormat contract.
            let stream = unsafe { *format.streams.add(index) };
            if stream.is_null() {
                return Err(SourceError::Discovery(format!(
                    "AVFormat returned a null stream at index {index}"
                )));
            }
            // SAFETY: the stream belongs to the live format context.
            match unsafe { TrackSnapshot::new(stream) }? {
                Some(snapshot) => {
                    discovered.push(snapshot.track.clone());
                    snapshots.push(Some(snapshot));
                }
                None => snapshots.push(None),
            }
        }

        let tracks = TrackCatalog::new(discovered)
            .map_err(|error| SourceError::Discovery(error.to_string()))?;
        Ok(Self {
            stream_count,
            tracks: snapshots,
            report: DiscoveryReport { tracks },
        })
    }

    pub fn report(&self) -> &DiscoveryReport {
        &self.report
    }

    /// Rejects a new stream or a changed codec configuration before its packet
    /// enters the Rust pipeline.
    ///
    /// # Safety
    ///
    /// Both pointers must belong to the same open context used for discovery.
    pub unsafe fn validate_packet(
        &self,
        context: *mut ffmpeg::AVFormatContext,
        packet: *const ffmpeg::AVPacket,
    ) -> Result<Option<TrackId>, SourceError> {
        // SAFETY: guaranteed by the caller.
        let format = unsafe { &*context };
        if usize::try_from(format.nb_streams).ok() != Some(self.stream_count) {
            return Err(SourceError::TrackSetChanged);
        }

        // SAFETY: guaranteed by the caller.
        let packet = unsafe { &*packet };
        let index = usize::try_from(packet.stream_index)
            .map_err(|_| SourceError::Input("packet has a negative stream index".into()))?;
        let snapshot = self
            .tracks
            .get(index)
            .ok_or_else(|| SourceError::Input("packet references an unknown stream".into()))?;
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        // SAFETY: the stream table is valid for `stream_count` entries.
        let stream = unsafe { *format.streams.add(index) };
        // SAFETY: both objects remain owned by AVFormat.
        if !unsafe { snapshot.matches(stream, packet) } {
            return Err(SourceError::CodecParametersChanged {
                track_id: snapshot.track.id,
            });
        }
        Ok(Some(snapshot.track.id))
    }
}

pub struct TrackSnapshot {
    track: DiscoveredTrack,
    codec_type: ffmpeg::AVMediaType,
    codec_id: ffmpeg::AVCodecID,
    codec_tag: u32,
    profile: i32,
    level: i32,
    width: i32,
    height: i32,
    sample_rate: i32,
    channels: i32,
    frame_size: i32,
    bits_per_raw_sample: i32,
    video_delay: i32,
    timebase: ffmpeg::AVRational,
    extradata: Payload,
}

impl TrackSnapshot {
    unsafe fn new(stream: *mut ffmpeg::AVStream) -> Result<Option<Self>, SourceError> {
        // SAFETY: guaranteed by the caller.
        let stream = unsafe { &*stream };
        if stream.codecpar.is_null() {
            return Err(SourceError::Discovery(format!(
                "stream {} has no codec parameters",
                stream.index
            )));
        }
        // SAFETY: codec parameters belong to the live stream.
        let parameters = unsafe { &*stream.codecpar };
        let media = match parameters.codec_type {
            ffmpeg::AVMediaType::AVMEDIA_TYPE_VIDEO => MediaParameters::Video {
                width: positive_u32(parameters.width, "video width")?,
                height: positive_u32(parameters.height, "video height")?,
                frame_rate: frame_rate(stream.avg_frame_rate)
                    .or_else(|| frame_rate(parameters.framerate)),
                video_delay: nonnegative_u32(parameters.video_delay, "video delay")?,
            },
            ffmpeg::AVMediaType::AVMEDIA_TYPE_AUDIO => MediaParameters::Audio {
                sample_rate: positive_u32(parameters.sample_rate, "audio sample rate")?,
                channels: positive_u16(parameters.ch_layout.nb_channels, "audio channels")?,
                frame_size: optional_u32(parameters.frame_size, "audio frame size")?,
                bit_depth: optional_u16(parameters.bits_per_raw_sample, "audio bit depth")?,
            },
            ffmpeg::AVMediaType::AVMEDIA_TYPE_SUBTITLE => MediaParameters::Subtitle,
            _ => return Ok(None),
        };
        let id = u32::try_from(stream.index)
            .map(TrackId)
            .map_err(|_| SourceError::Discovery("stream index is negative".into()))?;
        let timebase = timebase(stream.time_base)?;
        let extradata = Payload::from(extradata(parameters)?);
        Ok(Some(Self {
            track: DiscoveredTrack {
                id,
                codec: codec(parameters.codec_id),
                parameters: media,
                timebase,
                first_pts: (stream.start_time != ffmpeg::AV_NOPTS_VALUE)
                    .then_some(stream.start_time),
                // SAFETY: metadata belongs to this live stream.
                title: unsafe { dictionary::value(stream.metadata, c"title") },
                // SAFETY: metadata belongs to this live stream.
                language: unsafe { dictionary::value(stream.metadata, c"language") },
                codec_extradata: extradata.clone(),
            },
            codec_type: parameters.codec_type,
            codec_id: parameters.codec_id,
            codec_tag: parameters.codec_tag,
            profile: parameters.profile,
            level: parameters.level,
            width: parameters.width,
            height: parameters.height,
            sample_rate: parameters.sample_rate,
            channels: parameters.ch_layout.nb_channels,
            frame_size: parameters.frame_size,
            bits_per_raw_sample: parameters.bits_per_raw_sample,
            video_delay: parameters.video_delay,
            timebase: stream.time_base,
            extradata,
        }))
    }

    unsafe fn matches(&self, stream: *mut ffmpeg::AVStream, packet: &ffmpeg::AVPacket) -> bool {
        if stream.is_null() {
            return false;
        }
        // SAFETY: checked above and owned by the format context.
        let parameters = unsafe { (*stream).codecpar };
        if parameters.is_null() {
            return false;
        }
        // SAFETY: checked above.
        let parameters = unsafe { &*parameters };
        if self.codec_type != parameters.codec_type
            || self.codec_id != parameters.codec_id
            || self.codec_tag != parameters.codec_tag
            || self.profile != parameters.profile
            || self.level != parameters.level
            || self.width != parameters.width
            || self.height != parameters.height
            || self.sample_rate != parameters.sample_rate
            || self.channels != parameters.ch_layout.nb_channels
            || self.frame_size != parameters.frame_size
            || self.bits_per_raw_sample != parameters.bits_per_raw_sample
            || self.video_delay != parameters.video_delay
            || self.timebase != unsafe { (*stream).time_base }
            || !same_extradata(self.extradata.as_bytes(), parameters)
        {
            return false;
        }

        let mut parameter_change_size = 0_usize;
        // SAFETY: `packet` remains live for the duration of this call.
        let parameter_change = unsafe {
            ffmpeg::av_packet_get_side_data(
                packet,
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_PARAM_CHANGE,
                &mut parameter_change_size,
            )
        };
        if !parameter_change.is_null() {
            return false;
        }

        let mut side_data_size = 0_usize;
        // SAFETY: `packet` is live for this call and the returned slice is only
        // inspected before the packet is unreferenced.
        let side_data = unsafe {
            ffmpeg::av_packet_get_side_data(
                packet,
                ffmpeg::AVPacketSideDataType::AV_PKT_DATA_NEW_EXTRADATA,
                &mut side_data_size,
            )
        };
        side_data.is_null()
            || (
                // SAFETY: FFmpeg returned `side_data_size` readable bytes.
                unsafe { slice::from_raw_parts(side_data, side_data_size) }
                    == self.extradata.as_bytes()
            )
    }
}

fn codec(codec: ffmpeg::AVCodecID) -> Codec {
    match codec {
        ffmpeg::AVCodecID::AV_CODEC_ID_H264 => Codec::H264,
        ffmpeg::AVCodecID::AV_CODEC_ID_HEVC => Codec::Hevc,
        ffmpeg::AVCodecID::AV_CODEC_ID_AV1 => Codec::Av1,
        ffmpeg::AVCodecID::AV_CODEC_ID_AAC | ffmpeg::AVCodecID::AV_CODEC_ID_AAC_LATM => Codec::Aac,
        ffmpeg::AVCodecID::AV_CODEC_ID_OPUS => Codec::Opus,
        ffmpeg::AVCodecID::AV_CODEC_ID_WEBVTT => Codec::WebVtt,
        other => Codec::Unknown(other as u32),
    }
}

fn timebase(value: ffmpeg::AVRational) -> Result<Timebase, SourceError> {
    let numerator = positive_u32(value.num, "timebase numerator")?;
    let denominator = positive_u32(value.den, "timebase denominator")?;
    Ok(Timebase::new(numerator, denominator))
}

fn frame_rate(value: ffmpeg::AVRational) -> Option<FrameRate> {
    let numerator = u32::try_from(value.num).ok().and_then(NonZero::new)?;
    let denominator = u32::try_from(value.den).ok().and_then(NonZero::new)?;
    Some(FrameRate::new(numerator, denominator))
}

fn positive_u32(value: i32, field: &str) -> Result<NonZero<u32>, SourceError> {
    u32::try_from(value)
        .ok()
        .and_then(NonZero::new)
        .ok_or_else(|| SourceError::Discovery(format!("{field} is not positive")))
}

fn positive_u16(value: i32, field: &str) -> Result<NonZero<u16>, SourceError> {
    u16::try_from(value)
        .ok()
        .and_then(NonZero::new)
        .ok_or_else(|| SourceError::Discovery(format!("{field} is not positive")))
}

fn nonnegative_u32(value: i32, field: &str) -> Result<u32, SourceError> {
    u32::try_from(value).map_err(|_| SourceError::Discovery(format!("{field} is negative")))
}

fn optional_u32(value: i32, field: &str) -> Result<Option<NonZero<u32>>, SourceError> {
    if value == 0 {
        return Ok(None);
    }
    positive_u32(value, field).map(Some)
}

fn optional_u16(value: i32, field: &str) -> Result<Option<NonZero<u16>>, SourceError> {
    if value == 0 {
        return Ok(None);
    }
    positive_u16(value, field).map(Some)
}

fn extradata(parameters: &ffmpeg::AVCodecParameters) -> Result<Vec<u8>, SourceError> {
    let size = usize::try_from(parameters.extradata_size)
        .map_err(|_| SourceError::Discovery("codec extradata size is negative".into()))?;
    if size == 0 {
        return Ok(Vec::new());
    }
    if parameters.extradata.is_null() {
        return Err(SourceError::Discovery(
            "codec extradata pointer is null".into(),
        ));
    }
    // SAFETY: codec parameters promise `extradata_size` readable bytes.
    Ok(unsafe { slice::from_raw_parts(parameters.extradata, size) }.to_vec())
}

fn same_extradata(expected: &[u8], parameters: &ffmpeg::AVCodecParameters) -> bool {
    let Ok(size) = usize::try_from(parameters.extradata_size) else {
        return false;
    };
    if size == 0 {
        return expected.is_empty();
    }
    if parameters.extradata.is_null() || size != expected.len() {
        return false;
    }
    // SAFETY: codec parameters promise `extradata_size` readable bytes.
    unsafe { slice::from_raw_parts(parameters.extradata, size) == expected }
}
