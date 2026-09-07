//! In-process publishers the Apple HLS suite drives through `run_session`.

use std::{io::Cursor, sync::Arc};

use bytes::Bytes;
use rushls::{
    admission::{
        ClientInfo, IngestProtocol, PresentedCredential, PublishGrant, PublishRequest,
        PublishResource,
    },
    domain::{BoxFuture, DiscoveredTrack, MediaKind, TrackCatalog},
    observe::SourceMeters,
    source::{
        AcceptedPublish, DiscoveryLimits, DiscoveryReport, IngressEvent, InputLimits, InputState,
        MpegTsConfig, MpegTsPacketSource, Packet, PacketSource, PendingPublish, PublishRejection,
        ReadInput, RtmpPacketSource, SourceError, TransportError, channel,
    },
};

use crate::flv;

const H264_AAC_FLV: &[u8] = include_bytes!("fixtures/h264_aac.flv");
const HEVC_AAC_FLV: &[u8] = include_bytes!("fixtures/hevc_aac.flv");
pub const H264_AAC_TS: &[u8] = include_bytes!("fixtures/h264_aac.ts");
pub const HEVC_AAC_TS: &[u8] = include_bytes!("fixtures/hevc_aac.ts");
pub const H264_DUAL_AAC_TS: &[u8] = include_bytes!("fixtures/h264_dual_aac.ts");
pub const H264_DUAL_VIDEO_TS: &[u8] = include_bytes!("fixtures/h264_dual_video.ts");

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

pub fn rtmp_hevc_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events(HEVC_AAC_FLV)?, &[]))
}

pub fn rtmp_h264_two_aac() -> Result<Box<dyn PendingPublish>, String> {
    Ok(rtmp_events(flv::ingress_events_two_aac(H264_AAC_FLV)?, &[]))
}

pub fn rtmp_h264_aac_captions() -> Result<Box<dyn PendingPublish>, String> {
    let captions = [
        (0_u32, "first caption"),
        (2_000, "second caption"),
        (4_000, "third caption"),
        (6_000, "fourth caption"),
    ];
    Ok(rtmp_events(
        flv::ingress_events(H264_AAC_FLV)?,
        &captions.map(|(timestamp, text)| (timestamp, text.as_bytes().to_vec())),
    ))
}

pub fn mpegts(bytes: &'static [u8]) -> Box<dyn PendingPublish> {
    Box::new(LabeledPublish {
        inner: Box::new(MpegTsPublish {
            request: live_camera(IngestProtocol::Srt),
            bytes,
        }),
    })
}

pub fn labeled(inner: Box<dyn PendingPublish>) -> Box<dyn PendingPublish> {
    Box::new(LabeledPublish { inner })
}

fn rtmp_events(
    mut events: Vec<IngressEvent>,
    captions: &[(u32, Vec<u8>)],
) -> Box<dyn PendingPublish> {
    if !captions.is_empty() {
        events = interleave_captions(events, captions);
    }
    Box::new(LabeledPublish {
        inner: Box::new(RtmpPublish {
            request: live_camera(IngestProtocol::Rtmp),
            events,
        }),
    })
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

fn encode_cue(name: &[u8], text: &[u8]) -> Bytes {
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

struct LabeledPublish {
    inner: Box<dyn PendingPublish>,
}

impl PendingPublish for LabeledPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        self.inner.publish_request()
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let mut accepted = self.inner.accept(grant, meters).await?;
            accepted.source = Box::new(LabeledSource {
                inner: accepted.source,
            });
            Ok(accepted)
        })
    }

    fn reject(
        self: Box<Self>,
        rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        self.inner.reject(rejection)
    }
}

/// Stamps `LANGUAGE` after discovery only when the adapter left it unset.
///
/// MPEG-TS already copies PMT ISO 639 onto the catalog. RTMP and caption
/// tracks still have none, and Apple's reports want a tag when one exists.
struct LabeledSource {
    inner: Box<dyn PacketSource>,
}

impl PacketSource for LabeledSource {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            let mut report = self.inner.discover(limits).await?;
            stamp_languages(&mut report)?;
            Ok(report)
        })
    }

    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn rushls::domain::Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        self.inner.fill(out)
    }
}

fn stamp_languages(report: &mut DiscoveryReport) -> Result<(), SourceError> {
    let mut tracks: Vec<DiscoveredTrack> = report.tracks.tracks().to_vec();
    let mut audio = 0_usize;
    for track in &mut tracks {
        if track.language.is_some() {
            continue;
        }
        match track.kind() {
            MediaKind::Audio => {
                track.language = Some(if audio == 0 { "en" } else { "es" }.into());
                audio += 1;
            }
            MediaKind::Subtitle => track.language = Some("en".into()),
            MediaKind::Video => {}
        }
    }
    report.tracks = TrackCatalog::new(tracks)?;
    Ok(())
}
