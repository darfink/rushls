use std::{num::NonZero, slice};

use ffmpeg_sys_next as ffmpeg;

use crate::{
    domain::{
        Codec, DiscoveredTrack, FrameRate, MediaParameters, Payload, Timebase, TrackCatalog,
        TrackId,
    },
    ffmpeg::{from_av_rational, value, RationalError},
    source::{DiscoveryProblem, DiscoveryReport, SourceError},
};

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

        let mut snapshots = Vec::with_capacity(stream_count);
        let mut discovered = Vec::with_capacity(stream_count);
        for index in 0..stream_count {
            // SAFETY: `streams` has `nb_streams` entries by AVFormat contract.
            let stream = unsafe { *format.streams.add(index) };
            if stream.is_null() {
                return Err(DiscoveryProblem::Missing { field: "stream" }.into());
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

        let tracks = TrackCatalog::new(discovered)?;
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

/// Codec identity FFmpeg exposes that [`DiscoveredTrack`] has no field for.
///
/// Everything else the mid-stream check cares about — dimensions, sample rate,
/// channels, timebase, extradata — is already extracted into types that derive
/// [`PartialEq`], so this holds only the remainder rather than mirroring the
/// whole of `AVCodecParameters`.
///
/// The derive is the point. A hand-written comparison chain is one forgotten
/// `||` away from silently accepting the codec change this exists to reject,
/// and nothing would fail to compile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CodecIdentity {
    codec_type: ffmpeg::AVMediaType,
    codec_id: ffmpeg::AVCodecID,
    codec_tag: u32,
    profile: i32,
    level: i32,
}

impl CodecIdentity {
    /// # Safety
    ///
    /// `parameters` must belong to a live stream owned by the caller.
    fn read(parameters: &ffmpeg::AVCodecParameters) -> Self {
        Self {
            codec_type: parameters.codec_type,
            codec_id: parameters.codec_id,
            codec_tag: parameters.codec_tag,
            profile: parameters.profile,
            level: parameters.level,
        }
    }
}

pub struct TrackSnapshot {
    track: DiscoveredTrack,
    identity: CodecIdentity,
}

impl TrackSnapshot {
    unsafe fn new(stream: *mut ffmpeg::AVStream) -> Result<Option<Self>, SourceError> {
        // SAFETY: guaranteed by the caller.
        let stream = unsafe { &*stream };
        if stream.codecpar.is_null() {
            return Err(DiscoveryProblem::Missing {
                field: "codec parameters",
            }
            .into());
        }
        // SAFETY: codec parameters belong to the live stream.
        let parameters = unsafe { &*stream.codecpar };
        let Some(media) = media_parameters(parameters, declared_frame_rate(stream, parameters))?
        else {
            return Ok(None);
        };
        let id =
            u32::try_from(stream.index)
                .map(TrackId)
                .map_err(|_| DiscoveryProblem::Negative {
                    field: "stream index",
                })?;
        let timebase = timebase(stream.time_base)?;
        Ok(Some(Self {
            track: DiscoveredTrack {
                id,
                // AVStream indexes are publication-local. A protocol-aware
                // adapter may later supply a durable identity instead.
                source_key: None,
                codec: codec(parameters.codec_id),
                parameters: media,
                timebase,
                first_pts: (stream.start_time != ffmpeg::AV_NOPTS_VALUE)
                    .then_some(stream.start_time),
                // SAFETY: metadata belongs to this live stream.
                title: unsafe { value(stream.metadata, c"title") },
                // SAFETY: metadata belongs to this live stream.
                language: unsafe { value(stream.metadata, c"language") },
                codec_extradata: Payload::from(extradata(parameters)?),
            },
            identity: CodecIdentity::read(parameters),
        }))
    }

    /// Whether the stream still carries the configuration it was discovered
    /// with.
    ///
    /// Compares the extracted [`DiscoveredTrack`] fields structurally instead
    /// of re-listing FFmpeg's. Adding a field to [`MediaParameters`] therefore
    /// extends this check automatically; forgetting to mirror one is no longer
    /// something a reader has to notice.
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
        // SAFETY: the stream belongs to the live format context.
        let stream = unsafe { &*stream };

        // A parameter set this reader cannot describe is a change by
        // definition: discovery accepted a shape that is no longer there.
        let Ok(Some(media)) = media_parameters(parameters, declared_frame_rate(stream, parameters))
        else {
            return false;
        };
        let time_base = stream.time_base;
        if self.identity != CodecIdentity::read(parameters)
            || self.track.parameters != media
            || timebase(time_base).ok() != Some(self.track.timebase)
            || !same_extradata(self.track.codec_extradata.as_bytes(), parameters)
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
                    == self.track.codec_extradata.as_bytes()
            )
    }
}

/// Extracts the declared shape of a stream, or `None` for a kind this node
/// does not carry.
///
/// The single source of truth for both discovery and the mid-stream change
/// check, so the two can never disagree about what a stream's parameters are.
fn media_parameters(
    parameters: &ffmpeg::AVCodecParameters,
    frame_rate: Option<FrameRate>,
) -> Result<Option<MediaParameters>, SourceError> {
    let media = match parameters.codec_type {
        ffmpeg::AVMediaType::AVMEDIA_TYPE_VIDEO => MediaParameters::Video {
            width: positive_u32(parameters.width, "video width")?,
            height: positive_u32(parameters.height, "video height")?,
            frame_rate,
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
    Ok(Some(media))
}

/// The cadence a video stream declares, preferring the stream's own average.
///
/// Codec parameters carry a framerate too, but live inputs commonly leave it
/// unset while populating the stream average, so the stream wins.
///
/// # Safety
///
/// Both references must belong to the same live format context.
fn declared_frame_rate(
    stream: &ffmpeg::AVStream,
    parameters: &ffmpeg::AVCodecParameters,
) -> Option<FrameRate> {
    frame_rate(stream.avg_frame_rate).or_else(|| frame_rate(parameters.framerate))
}

fn codec(codec: ffmpeg::AVCodecID) -> Codec {
    match codec {
        ffmpeg::AVCodecID::AV_CODEC_ID_H264 => Codec::H264,
        ffmpeg::AVCodecID::AV_CODEC_ID_HEVC => Codec::Hevc,
        ffmpeg::AVCodecID::AV_CODEC_ID_AV1 => Codec::Av1,
        ffmpeg::AVCodecID::AV_CODEC_ID_AAC | ffmpeg::AVCodecID::AV_CODEC_ID_AAC_LATM => Codec::Aac,
        ffmpeg::AVCodecID::AV_CODEC_ID_OPUS => Codec::Opus,
        ffmpeg::AVCodecID::AV_CODEC_ID_WEBVTT => Codec::WebVtt,
        ffmpeg::AVCodecID::AV_CODEC_ID_MOV_TEXT => Codec::MovText,
        other => Codec::Unknown(other as u32),
    }
}

fn timebase(value: ffmpeg::AVRational) -> Result<Timebase, SourceError> {
    from_av_rational(value).map_err(|error| {
        DiscoveryProblem::NotPositive {
            field: match error {
                RationalError::Numerator => "timebase numerator",
                RationalError::Denominator => "timebase denominator",
            },
        }
        .into()
    })
}

fn frame_rate(value: ffmpeg::AVRational) -> Option<FrameRate> {
    let numerator = u32::try_from(value.num).ok().and_then(NonZero::new)?;
    let denominator = u32::try_from(value.den).ok().and_then(NonZero::new)?;
    Some(FrameRate::new(numerator, denominator))
}

// FFmpeg reports every declared scalar as `i32`, using zero or a negative
// value for "not declared". Narrowing therefore needs the same two checks
// everywhere and differs only in target width. A generic over `NonZero<T>`
// would need the unstable `ZeroablePrimitive` bound, so the two widths stay
// spelled out; the `optional_*` pair delegates rather than repeating them.

fn positive_u32(value: i32, field: &'static str) -> Result<NonZero<u32>, SourceError> {
    u32::try_from(value)
        .ok()
        .and_then(NonZero::new)
        .ok_or(DiscoveryProblem::NotPositive { field }.into())
}

fn positive_u16(value: i32, field: &'static str) -> Result<NonZero<u16>, SourceError> {
    u16::try_from(value)
        .ok()
        .and_then(NonZero::new)
        .ok_or(DiscoveryProblem::NotPositive { field }.into())
}

fn nonnegative_u32(value: i32, field: &'static str) -> Result<u32, SourceError> {
    u32::try_from(value).map_err(|_| DiscoveryProblem::Negative { field }.into())
}

fn optional_u32(value: i32, field: &'static str) -> Result<Option<NonZero<u32>>, SourceError> {
    if value == 0 {
        return Ok(None);
    }
    positive_u32(value, field).map(Some)
}

fn optional_u16(value: i32, field: &'static str) -> Result<Option<NonZero<u16>>, SourceError> {
    if value == 0 {
        return Ok(None);
    }
    positive_u16(value, field).map(Some)
}

fn extradata(parameters: &ffmpeg::AVCodecParameters) -> Result<Vec<u8>, SourceError> {
    let size =
        usize::try_from(parameters.extradata_size).map_err(|_| DiscoveryProblem::Negative {
            field: "codec extradata size",
        })?;
    if size == 0 {
        return Ok(Vec::new());
    }
    if parameters.extradata.is_null() {
        return Err(DiscoveryProblem::Missing {
            field: "codec extradata",
        }
        .into());
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
