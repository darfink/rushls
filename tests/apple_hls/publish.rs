//! In-process publishers the Apple HLS suite drives through `run_session`.

use std::{io::Cursor, sync::Arc};

use bytes::Bytes;
use rushls::{
    admission::{
        ClientInfo, IngestProtocol, PresentedCredential, PublishGrant, PublishRequest,
        PublishResource,
    },
    domain::BoxFuture,
    observe::SourceMeters,
    source::{
        AcceptedPublish, IngressEvent, InputLimits, InputState, MpegTsConfig, MpegTsPacketSource,
        PendingPublish, PublishRejection, ReadInput, RtmpPacketSource, TransportError, channel,
    },
};

use crate::{
    flv,
    shape::{Shape, shaped},
};

const H264_AAC_FLV: &[u8] = include_bytes!("fixtures/h264_aac.flv");
const H264_BFRAMES_AAC_FLV: &[u8] = include_bytes!("fixtures/h264_bframes_aac.flv");
const HEVC_AAC_FLV: &[u8] = include_bytes!("fixtures/hevc_aac.flv");
const H264_BPYRAMID_AAC_FLV: &[u8] = include_bytes!("fixtures/h264_bpyramid_aac.flv");
const H264_HE_AAC_FLV: &[u8] = include_bytes!("fixtures/h264_he_aac.flv");
const H264_AAC_441_MONO_FLV: &[u8] = include_bytes!("fixtures/h264_aac_441_mono.flv");
const H264_AAC_51_FLV: &[u8] = include_bytes!("fixtures/h264_aac_51.flv");
pub const H264_AAC_TS: &[u8] = include_bytes!("fixtures/h264_aac.ts");
pub const HEVC_AAC_TS: &[u8] = include_bytes!("fixtures/hevc_aac.ts");
pub const H264_DUAL_AAC_TS: &[u8] = include_bytes!("fixtures/h264_dual_aac.ts");
pub const H264_DUAL_VIDEO_TS: &[u8] = include_bytes!("fixtures/h264_dual_video.ts");
pub const H264_2398_AAC_TS: &[u8] = include_bytes!("fixtures/h264_2398_aac.ts");
pub const H264_LADDER3_AAC_TS: &[u8] = include_bytes!("fixtures/h264_ladder3_aac.ts");
pub const HEVC_HDR10_AAC_TS: &[u8] = include_bytes!("fixtures/hevc_hdr10_aac.ts");
pub const H264_MULTILANG_AAC_TS: &[u8] = include_bytes!("fixtures/h264_multilang_aac.ts");
pub const H264_LONGGOP_AAC_TS: &[u8] = include_bytes!("fixtures/h264_longgop_aac.ts");
pub const H264_ANAMORPHIC_AAC_TS: &[u8] = include_bytes!("fixtures/h264_anamorphic_aac.ts");
pub const H264_OPUS_TS: &[u8] = include_bytes!("fixtures/h264_opus.ts");
pub const AV1_TS: &[u8] = include_bytes!("fixtures/av1_long.ts");

/// Queue large enough to hold a whole fixture so accept can enqueue then finish.
fn ingress_capacity() -> std::num::NonZeroUsize {
    nz::usize!(2 * 1024 * 1024)
}

pub fn live_camera(protocol: IngestProtocol) -> PublishRequest {
    PublishRequest {
        protocol,
        resource: PublishResource {
            namespace: Some("live".into()),
            name: "camera".into(),
        },
        credential: PresentedCredential::new("secret"),
        client: ClientInfo {
            remote_address: "127.0.0.1:1935".parse().expect("constant is valid"),
            encoder: Some("apple-hls-fixture".into()),
            protocol_version: None,
        },
    }
}

pub fn rtmp_h264_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events(H264_AAC_FLV)?, &[]))
}

/// Reordered pictures on a keyframe period that is not a whole number of
/// container ticks: 75 frames of 30000/1001 alternates between 2502 and 2503
/// milliseconds, so a planned period fixed at either one is wrong every other
/// segment. The boundary window has to absorb that in both directions.
pub fn rtmp_h264_bframes_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events(H264_BFRAMES_AAC_FLV)?, &[]))
}

pub fn rtmp_hevc_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events(HEVC_AAC_FLV)?, &[]))
}

/// Eight consecutive B-frames under a normal pyramid: the composition offset
/// on some access units is seven frame periods, so decode order and
/// presentation order disagree far beyond one GOP's lookahead.
pub fn rtmp_h264_bpyramid_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(
        flv::ingress_events(H264_BPYRAMID_AAC_FLV)?,
        &[],
    ))
}

/// HE-AAC: the decoder output rate is twice the encoded frame rate, and the
/// RFC 6381 string is `mp4a.40.5` rather than `mp4a.40.2`.
pub fn rtmp_h264_he_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events(H264_HE_AAC_FLV)?, &[]))
}

/// 44.1 kHz mono. The audio grid shares no common period with a 30 fps video
/// timescale, so no segment boundary is exact in both.
pub fn rtmp_h264_aac_441_mono() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(
        flv::ingress_events(H264_AAC_441_MONO_FLV)?,
        &[],
    ))
}

/// Audio that starts 133 ms after the first keyframe, as encoders that
/// buffer audio commonly publish. Its first segment is shorter than the
/// video's; every later one spans the full cadence on the shared grid, and
/// the leading silence is an empty edit, not a shifted timeline.
pub fn rtmp_h264_late_aac() -> Result<Box<dyn PendingPublish>, String> {
    let mut events = flv::ingress_events(H264_AAC_FLV)?;
    for event in &mut events {
        if let IngressEvent::Audio { timestamp, .. } = event {
            *timestamp += 133;
        }
    }
    Ok(rtmp_events(events, &[]))
}

/// 5.1 audio, which the audio rendition must advertise as `CHANNELS="6"`.
pub fn rtmp_h264_aac_51() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events(H264_AAC_51_FLV)?, &[]))
}

/// The same media with the catalog reshaped, for topologies no adapter emits.
pub fn rtmp_shaped_h264_aac(shape: Shape) -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_shaped(flv::ingress_events(H264_AAC_FLV)?, shape))
}

/// H.264 and AAC with WebVTT cues, reshaped before the session sees it.
pub fn rtmp_shaped_h264_aac_captions(shape: Shape) -> Result<Box<dyn PendingPublish>, String> {
    let mut events = flv::ingress_events(H264_AAC_FLV)?;
    Ok(rtmp_shaped(
        events_with_captions(&mut events, &caption_cues()),
        shape,
    ))
}

/// Cues spread across the publication so every segment carries at least one.
fn caption_cues() -> Vec<(u32, Vec<u8>)> {
    (0..8)
        .map(|index| (index * 1_000, format!("caption at {index} s").into_bytes()))
        .collect()
}

pub fn rtmp_h264_two_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events_two_aac(H264_AAC_FLV)?, &[]))
}

pub fn rtmp_h264_aac_captions() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(
        flv::ingress_events(H264_AAC_FLV)?,
        &caption_cues(),
    ))
}

pub fn mpegts(bytes: &'static [u8]) -> Box<dyn PendingPublish> {
    mpegts_shaped(bytes, Shape::labelled())
}

pub fn mpegts_shaped(bytes: &'static [u8], shape: Shape) -> Box<dyn PendingPublish> {
    shaped(
        Box::new(MpegTsPublish {
            request: live_camera(IngestProtocol::Srt),
            bytes,
        }),
        shape,
    )
}

pub fn labeled(inner: Box<dyn PendingPublish>) -> Box<dyn PendingPublish> {
    shaped(inner, Shape::labelled())
}

fn rtmp_events(
    mut events: Vec<IngressEvent>,
    captions: &[(u32, Vec<u8>)],
) -> Box<dyn PendingPublish> {
    rtmp_shaped(
        events_with_captions(&mut events, captions),
        Shape::labelled(),
    )
}

/// Same media, with the catalog reshaped before the session sees it.
pub fn rtmp_shaped(events: Vec<IngressEvent>, shape: Shape) -> Box<dyn PendingPublish> {
    shaped(
        Box::new(RtmpPublish {
            request: live_camera(IngestProtocol::Rtmp),
            events,
        }),
        shape,
    )
}

fn events_with_captions(
    events: &mut Vec<IngressEvent>,
    captions: &[(u32, Vec<u8>)],
) -> Vec<IngressEvent> {
    if !captions.is_empty() {
        *events = interleave_captions(std::mem::take(events), captions);
    }
    std::mem::take(events)
}

fn interleave_captions(media: Vec<IngressEvent>, captions: &[(u32, Vec<u8>)]) -> Vec<IngressEvent> {
    let mut events = Vec::with_capacity(media.len() + captions.len());
    let mut emitted = 0;
    for event in media {
        let timestamp = match &event {
            IngressEvent::Audio { timestamp, .. } | IngressEvent::Video { timestamp, .. } => {
                *timestamp
            }
            IngressEvent::Script { timestamp, .. } => *timestamp,
            _ => 0,
        };
        while emitted < captions.len() && captions[emitted].0 <= timestamp {
            events.push(IngressEvent::Script {
                timestamp: captions[emitted].0,
                payload: encode_cue(b"onCaption", &captions[emitted].1),
            });
            emitted += 1;
        }
        events.push(event);
    }
    while emitted < captions.len() {
        events.push(IngressEvent::Script {
            timestamp: captions[emitted].0,
            payload: encode_cue(b"onCaption", &captions[emitted].1),
        });
        emitted += 1;
    }
    events
}

pub fn encode_cue(name: &[u8], text: &[u8]) -> Bytes {
    let mut payload = Vec::with_capacity(16 + name.len() + text.len());
    payload.push(0x02);
    payload.extend_from_slice(&u16::try_from(name.len()).expect("name fits").to_be_bytes());
    payload.extend_from_slice(name);
    payload.push(0x08);
    payload.extend_from_slice(&1_u32.to_be_bytes());
    payload.extend_from_slice(&4_u16.to_be_bytes());
    payload.extend_from_slice(b"text");
    payload.push(0x02);
    payload.extend_from_slice(&u16::try_from(text.len()).expect("text fits").to_be_bytes());
    payload.extend_from_slice(text);
    payload.extend_from_slice(&[0x00, 0x00, 0x09]);
    Bytes::from(payload)
}

struct RtmpPublish {
    request: PublishRequest,
    events: Vec<IngressEvent>,
}

impl PendingPublish for RtmpPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(self.request.clone())
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let (reader, writer) = channel(ingress_capacity());
            for event in self.events {
                writer
                    .send(event)
                    .await
                    .map_err(|error| TransportError::Accept(error.to_string().into()))?;
            }
            writer.finish(InputState::Closed);
            let source = RtmpPacketSource::new(reader, InputLimits::permissive(), meters)
                .map_err(|error| TransportError::Accept(error.to_string().into()))?;
            Ok(AcceptedPublish {
                source: Box::new(source),
                grant,
            })
        })
    }

    fn reject(
        self: Box<Self>,
        _rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async { Ok(()) })
    }
}

struct MpegTsPublish {
    request: PublishRequest,
    bytes: &'static [u8],
}

impl PendingPublish for MpegTsPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(self.request.clone())
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let source = MpegTsPacketSource::new(
                Box::new(ReadInput::closed(Cursor::new(self.bytes))),
                MpegTsConfig::default(),
                InputLimits::permissive(),
                meters,
            )
            .map_err(|error| TransportError::Accept(error.to_string().into()))?;
            Ok(AcceptedPublish {
                source: Box::new(source),
                grant,
            })
        })
    }

    fn reject(
        self: Box<Self>,
        _rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async { Ok(()) })
    }
}
