//! Real Enhanced RTMP publication of a bounded, repeatable encoded period.
use std::{io::IoSlice, net::SocketAddr, path::Path, time::Duration};

use bytes::Bytes;
use rtmpx::{
    DropPolicy, EnhancedValidationMode, Packet, Segments, ValidatedMedia,
    amf0::{Amf0Object, Amf0Value},
    handshake::{Handshake, HandshakeProgress, HandshakeRole},
    sessions::{
        ClientEvent, ClientOutput, ClientSession, ClientSessionConfig, DataMessage,
        DataMessageType, PublishMode, StreamHandle,
    },
    time::RtmpTimestamp,
};
use rushls::source::IngressEvent;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::TestResult;

type Error = Box<dyn std::error::Error + Send + Sync>;
pub const PERIOD_MS: u32 = 32_000;
pub const VIDEO_COUNT: usize = 6;
pub const AUDIO_COUNT: usize = 2;

pub struct Track {
    video: bool,
    id: u8,
    header: Bytes,
    samples: Vec<Bytes>,
}

impl Track {
    fn load(data: &[u8], video: bool, id: u8, count: usize) -> Result<Self, Error> {
        let mut header = None;
        let mut samples = Vec::new();
        for event in crate::flv::ingress_events(data)? {
            let raw = match event {
                IngressEvent::Video { media, .. } if video => media.raw().clone(),
                IngressEvent::Audio { media, .. } if !video => media.raw().clone(),
                _ => continue,
            };
            let packet_type = raw[1];
            if packet_type > 1 {
                continue;
            }
            let mut enhanced = vec![
                if video {
                    0x80 | (raw[0] & 0x70) | 6
                } else {
                    0x95
                },
                packet_type,
            ];
            enhanced.extend_from_slice(if video { b"avc1" } else { b"mp4a" });
            enhanced.push(id);
            // AVC sequence headers omit the legacy composition offset; coded
            // frames retain it. AAC has no composition offset in either form.
            enhanced.extend_from_slice(&raw[if video && packet_type == 0 { 5 } else { 2 }..]);
            let enhanced = Bytes::from(enhanced);
            if video {
                ValidatedMedia::parse_video(enhanced.clone(), EnhancedValidationMode::Strict)?;
            } else {
                ValidatedMedia::parse_audio(enhanced.clone(), EnhancedValidationMode::Strict)?;
            }
            if packet_type == 0 {
                header = Some(enhanced);
            } else {
                samples.push(enhanced);
            }
        }
        // Discard AAC encoder priming. A period is exactly 1500 AAC frames and
        // 960 video frames: both clocks meet at 32 s without cumulative drift.
        if !video && !samples.is_empty() {
            samples.remove(0);
        }
        if samples.len() < count {
            return Err(format!("track {id} has {} samples, need {count}", samples.len()).into());
        }
        samples.truncate(count);
        Ok(Self {
            video,
            id,
            header: header.ok_or("missing sequence header")?,
            samples,
        })
    }
}

pub fn load(directory: &Path) -> Result<Vec<Track>, Error> {
    let mut tracks = Vec::new();
    for index in 0..VIDEO_COUNT {
        tracks.push(Track::load(
            &std::fs::read(directory.join(format!("video-{index}.flv")))?,
            true,
            u8::try_from(index + 1)?,
            (PERIOD_MS * 30 / 1000) as usize,
        )?);
    }
    for index in 0..AUDIO_COUNT {
        tracks.push(Track::load(
            &std::fs::read(directory.join(format!("audio-{index}-96.flv")))?,
            false,
            u8::try_from(index + 1)?,
            (PERIOD_MS * 48 / 1024) as usize,
        )?);
    }
    Ok(tracks)
}

fn metadata(tracks: &[Track]) -> Result<Bytes, Error> {
    let mut video = Amf0Object::new();
    let mut audio = Amf0Object::new();
    for track in tracks {
        let mut values = Amf0Object::new();
        if !track.video {
            values.insert(
                "language".into(),
                Amf0Value::Utf8String(if track.id == 1 { "en" } else { "es" }.into()),
            );
            values.insert(
                "title".into(),
                Amf0Value::Utf8String(
                    if track.id == 1 {
                        "English test tone"
                    } else {
                        "Spanish test tone"
                    }
                    .into(),
                ),
            );
        }
        if track.video { &mut video } else { &mut audio }
            .insert(track.id.to_string(), Amf0Value::Object(values));
    }
    Ok(rtmpx::amf0::serialize(&[
        Amf0Value::Utf8String("onMetaData".into()),
        Amf0Value::Object(Amf0Object::from([
            ("language".into(), Amf0Value::Utf8String("en".into())),
            ("videoTrackIdInfoMap".into(), Amf0Value::Object(video)),
            ("audioTrackIdInfoMap".into(), Amf0Value::Object(audio)),
        ])),
    ])?
    .into())
}

async fn write_packet<P: Segments>(
    socket: &mut TcpStream,
    mut packet: Packet<P>,
) -> std::io::Result<()> {
    while !packet.is_complete() {
        let mut slices = [IoSlice::new(&[]); 32];
        let count = packet.io_slices(&mut slices);
        let written = socket.write_vectored(&slices[..count]).await?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        packet.advance(written);
    }
    Ok(())
}

async fn connect(address: SocketAddr) -> Result<(TcpStream, ClientSession, StreamHandle), Error> {
    let mut socket = TcpStream::connect(address).await?;
    socket.set_nodelay(true)?;
    let mut handshake = Handshake::new(HandshakeRole::Client);
    socket
        .write_all(&handshake.generate_outbound_p0_and_p1()?)
        .await?;
    let mut buffer = vec![0; 16384];
    let mut input = loop {
        let n = socket.read(&mut buffer).await?;
        if n == 0 {
            return Err("RTMP peer closed during handshake".into());
        }
        match handshake.process_bytes(&buffer[..n])? {
            HandshakeProgress::InProgress { response_bytes } => {
                socket.write_all(&response_bytes).await?;
            }
            HandshakeProgress::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                socket.write_all(&response_bytes).await?;
                break Bytes::from(remaining_bytes);
            }
        }
    };
    let mut session = ClientSession::new(ClientSessionConfig::default())?;
    session.connect("live")?;
    let mut accepted = None;
    loop {
        while let Some(output) = session.receive(&mut input)? {
            match output {
                ClientOutput::Packet(packet) => write_packet(&mut socket, packet).await?,
                ClientOutput::Event(ClientEvent::ConnectionRequestAccepted { .. }) => {
                    session.publish("camera", PublishMode::Live)?;
                }
                ClientOutput::Event(ClientEvent::PublishRequestAccepted { stream, .. }) => {
                    accepted = Some(stream);
                }
                ClientOutput::Event(ClientEvent::ConnectionRequestRejected {
                    description, ..
                }) => return Err(description.into()),
                ClientOutput::Event(ClientEvent::PublishRequestRejected { status, .. }) => {
                    return Err(format!("RTMP publish rejected: {status:?}").into());
                }
                _ => {}
            }
        }
        if let Some(stream) = accepted {
            return Ok((socket, session, stream));
        }
        let n = socket.read(&mut buffer).await?;
        if n == 0 {
            return Err("RTMP peer closed before publication".into());
        }
        input = Bytes::copy_from_slice(&buffer[..n]);
    }
}

async fn send(
    socket: &mut TcpStream,
    session: &mut ClientSession,
    stream: StreamHandle,
    video: bool,
    timestamp: u32,
    bytes: Bytes,
) -> TestResult {
    let packet = if video {
        session.send_video(
            stream,
            bytes,
            RtmpTimestamp::new(timestamp),
            DropPolicy::Never,
        )?
    } else {
        session.send_audio(
            stream,
            bytes,
            RtmpTimestamp::new(timestamp),
            DropPolicy::Never,
        )?
    };
    write_packet(socket, packet).await?;
    Ok(())
}

/// Burst the DVR history, then keep publishing at wall-clock speed until aborted.
/// All timing is in the producer: cancelling an origin read cannot drop packets.
pub async fn publish(
    address: SocketAddr,
    tracks: Vec<Track>,
    prefill_ms: u32,
    ready: tokio::sync::oneshot::Sender<()>,
) -> TestResult {
    let (mut socket, mut session, stream) = connect(address).await?;
    write_packet(
        &mut socket,
        session.send_data(
            stream,
            DataMessage::new(
                DataMessageType::Amf0,
                RtmpTimestamp::new(0),
                metadata(&tracks)?,
            ),
        )?,
    )
    .await?;
    for track in &tracks {
        send(
            &mut socket,
            &mut session,
            stream,
            track.video,
            0,
            track.header.clone(),
        )
        .await?;
    }
    let mut positions = vec![0_u64; tracks.len()];
    let mut caption = 0_u32;
    let mut wall = None;
    let mut ready = Some(ready);
    let mut buffer = vec![0; 16384];
    loop {
        let (index, stamp) = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let ms = if t.video {
                    positions[i] * 1000 / 30
                } else {
                    positions[i] * 1024 * 1000 / 48000
                };
                (
                    i,
                    u32::try_from(ms).expect("fixture shorter than RTMP timestamp wrap"),
                )
            })
            .min_by_key(|(_, stamp)| *stamp)
            .ok_or("no fixture tracks")?;
        if stamp >= prefill_ms {
            let wall = *wall.get_or_insert_with(tokio::time::Instant::now);
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            tokio::time::sleep_until(wall + Duration::from_millis(u64::from(stamp - prefill_ms)))
                .await;
        }
        // Read acknowledgements and answer pings throughout the long publication.
        match socket.try_read(&mut buffer) {
            Ok(0) => return Err("RTMP peer closed during publication".into()),
            Ok(n) => {
                let mut input = Bytes::copy_from_slice(&buffer[..n]);
                while let Some(output) = session.receive(&mut input)? {
                    if let ClientOutput::Packet(packet) = output {
                        write_packet(&mut socket, packet).await?;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if stamp >= caption {
            let text = format!("Synthetic caption at {} seconds", caption / 1000);
            write_packet(
                &mut socket,
                session.send_data(
                    stream,
                    DataMessage::new(
                        DataMessageType::Amf0,
                        RtmpTimestamp::new(caption),
                        crate::publish::encode_cue(b"onCaption", text.as_bytes()),
                    ),
                )?,
            )
            .await?;
            caption += 1000;
        }
        let track = &tracks[index];
        let sample = usize::try_from(positions[index] % track.samples.len() as u64)?;
        send(
            &mut socket,
            &mut session,
            stream,
            track.video,
            stamp,
            track.samples[sample].clone(),
        )
        .await?;
        positions[index] += 1;
    }
}

#[tokio::test]
async fn enhanced_rtmp_discovers_variants_renditions_and_script_captions() -> TestResult {
    use rushls::{
        admission::{Principal, PublishGrant, StreamPolicy},
        domain::{MediaKind, StreamId},
        observe::{ProcessMeters, SessionMeters},
        source::{
            DiscoveryLimits, PendingPublish,
            transport::rtmp::{RtmpConfig, RtmpPendingPublish},
        },
    };
    // Exercise the wire protocol in the ordinary test suite without FFmpeg or
    // Apple tools. The authoring audit supplies separately encoded resolutions.
    let media = include_bytes!("../fixtures/h264_aac.flv");
    let tracks = vec![
        Track::load(media, true, 1, 30)?,
        Track::load(media, true, 2, 30)?,
        Track::load(media, false, 1, 47)?,
        Track::load(media, false, 2, 47)?,
    ];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let (ready, _) = tokio::sync::oneshot::channel();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(publish(listener.local_addr()?, tracks, 2000, ready));
    tokio::time::timeout(Duration::from_secs(10), async {
        let (socket, _) = listener.accept().await?;
        let pending = RtmpPendingPublish::handshake_tcp(socket, RtmpConfig::default()).await?;
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut accepted = Box::new(pending)
            .accept(
                PublishGrant {
                    stream_id: StreamId::new("live/camera"),
                    principal: Principal("fixture".into()),
                    policy: StreamPolicy::permissive(),
                },
                meters.source_view(),
            )
            .await?;
        let discovered = accepted
            .source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 2 * 1024 * 1024,
                maximum_wall_time: Duration::from_secs(5),
            })
            .await?;
        let tracks = discovered.tracks.tracks();
        assert_eq!(
            tracks
                .iter()
                .filter(|t| t.kind() == MediaKind::Video)
                .count(),
            2
        );
        let audio: Vec<_> = tracks
            .iter()
            .filter(|t| t.kind() == MediaKind::Audio)
            .collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].language.as_deref(), Some("en"));
        assert_eq!(audio[1].language.as_deref(), Some("es"));
        let text = tracks
            .iter()
            .find(|t| t.kind() == MediaKind::Subtitle)
            .ok_or("onCaption did not create a subtitle track")?;
        assert_eq!(text.language.as_deref(), Some("en"));
        let mut found = false;
        for _ in 0..100 {
            let mut packets = Vec::new();
            accepted.source.fill(&mut packets).await?;
            found |= packets.iter().any(|p| {
                p.track_id == text.id && p.payload.as_bytes() == b"Synthetic caption at 0 seconds"
            });
            if found {
                break;
            }
        }
        assert!(found, "onCaption text did not survive RTMP ingest");
        Ok::<(), Error>(())
    })
    .await??;
    tasks.abort_all();
    Ok(())
}
