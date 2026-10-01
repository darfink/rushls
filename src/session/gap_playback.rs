//! Opt-in browser fixture through the real session, HTTP origin, and store.
//! Uses production normalization and packaging, with controlled source packet loss.
use crate::{
    admission::{
        ClientInfo, IngestProtocol, OpenStreamAuthenticator, PresentedCredential, PublishGrant,
        PublishRequest, PublishResource, StreamPolicy,
    },
    domain::{Appender, BoxFuture, MediaKind, TrackId},
    observe::{Events, SourceMeters},
    server::{
        Node, NodeConfig,
        http::{HttpConfig, Readiness, serve},
    },
    source::{
        AcceptedPublish, DiscoveryLimits, DiscoveryReport, InputLimits, InputState, MpegTsConfig,
        MpegTsPacketSource, Packet, PacketSource, PendingPublish, PublishRejection, ReadInput,
        SourceError, TransportError,
    },
};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Cursor,
    sync::Arc,
    time::Duration,
};

#[derive(Default)]
struct FixtureEvents(parking_lot::Mutex<Vec<crate::observe::SessionEvent>>);
impl crate::observe::EventObserver for FixtureEvents {
    fn observe(&self, _: crate::domain::SessionId, event: crate::observe::SessionEvent) {
        self.0.lock().push(event);
    }
}

struct Publish(Vec<u8>);
impl PendingPublish for Publish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(PublishRequest {
            protocol: IngestProtocol::Srt,
            resource: PublishResource {
                namespace: Some("live".into()),
                name: "gap-probe".into(),
            },
            credential: PresentedCredential::new("fixture"),
            client: ClientInfo {
                remote_address: "127.0.0.1:1935".parse().expect("constant address"),
                encoder: Some("gap-player-fixture".into()),
                protocol_version: None,
            },
        })
    }
    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let source = MpegTsPacketSource::new(
                Box::new(ReadInput::closed(Cursor::new(self.0))),
                MpegTsConfig::default(),
                InputLimits::permissive(),
                meters,
            )
            .map_err(|e| TransportError::Accept(e.to_string().into()))?;
            Ok(AcceptedPublish {
                source: Box::new(Holes {
                    source,
                    audio: BTreeMap::new(),
                    video: BTreeMap::new(),
                    video_scenario: std::env::var("RUSHLS_GAP_LIVE_VIDEO")
                        .ok()
                        .filter(|_| std::env::var_os("RUSHLS_GAP_LIVE_CONTROL").is_none()),
                    pending: VecDeque::new(),
                    clocks: BTreeMap::new(),
                    state: InputState::Open,
                    anchor: None,
                    gaps: std::env::var_os("RUSHLS_GAP_LIVE_CONTROL").is_none()
                        && std::env::var("RUSHLS_GAP_LIVE_VIDEO")
                            .map_or(true, |scenario| scenario == "combined"),
                }),
                grant,
            })
        })
    }
    fn reject(
        self: Box<Self>,
        _: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async { Ok(()) })
    }
}

struct Holes {
    source: MpegTsPacketSource,
    audio: BTreeMap<TrackId, (usize, usize)>,
    video: BTreeMap<TrackId, Option<i64>>,
    video_scenario: Option<String>,
    pending: VecDeque<Packet>,
    clocks: BTreeMap<TrackId, crate::domain::Timebase>,
    state: InputState,
    anchor: Option<(i64, tokio::time::Instant)>,
    gaps: bool,
}
impl PacketSource for Holes {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            let report = self.source.discover(limits).await?;
            for track in report.tracks.tracks() {
                self.clocks.insert(track.id, track.timebase);
                if track.kind() == MediaKind::Audio {
                    self.audio.insert(track.id, (0, self.audio.len() * 25));
                } else if track.kind() == MediaKind::Video {
                    self.video.insert(track.id, None);
                }
            }
            Ok(report)
        })
    }
    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        Box::pin(async move {
            if self.pending.is_empty() && self.state == InputState::Open {
                let mut packets = Vec::new();
                self.state = self.source.fill(&mut packets).await?;
                self.pending.extend(packets);
            }
            if let Some(packet) = self.pending.front() {
                // Audio uses its sample clock after demuxing. Normalize pacing
                // to 90 kHz so clients observe correctly paced open parents
                // and blocking reloads, not a VOD copy.
                if let Some(raw) = packet.dts.or(packet.pts) {
                    let clock = self.clocks[&packet.track_id];
                    let stamp =
                        raw * i64::from(clock.num().get()) * 90_000 / i64::from(clock.den().get());
                    let (first, wall) = *self
                        .anchor
                        .get_or_insert((stamp, tokio::time::Instant::now()));
                    let elapsed = u64::try_from(stamp.saturating_sub(first)).unwrap_or(0);
                    tokio::time::sleep_until(
                        wall + Duration::from_secs(elapsed / 90_000)
                            + Duration::from_nanos((elapsed % 90_000) * 1_000_000_000 / 90_000),
                    )
                    .await;
                }
                // Session supervision can cancel a fill while it waits. Keep
                // the packet queued until pacing completes, or that cancellation
                // creates unintended source loss in an otherwise clean fixture.
                let packet = self.pending.pop_front().expect("paced packet is queued");
                let mut missing = false;
                if let Some((count, offset)) = self.audio.get_mut(&packet.track_id) {
                    // One small hole and one spanning two parts, after pre-roll.
                    missing = (250 + *offset..256 + *offset).contains(count)
                        || (360 + *offset..380 + *offset).contains(count);
                    *count += 1;
                }
                let audio_missing = missing && self.gaps;
                let mut video_missing = false;
                if let Some(origin) = self.video.get_mut(&packet.track_id) {
                    let pts = packet.pts.expect("fixture video has PTS");
                    let offset = pts - *origin.get_or_insert(pts);
                    video_missing = self.video_scenario.as_ref().is_some_and(|scenario| {
                        offset == if scenario == "early" { 36_000 } else { 453_600 }
                            || (scenario != "single" && matches!(offset, 705_600 | 1_173_600))
                    });
                    if video_missing {
                        assert!(!packet.random_access);
                    }
                }
                if !audio_missing && !video_missing {
                    out.push(packet);
                }
            }
            Ok(if self.pending.is_empty() {
                self.state
            } else {
                InputState::Open
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires ffmpeg, RUSHLS_GAP_LIVE_READY, and an external browser probe"]
async fn live_av_gap_browser_origin() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ready = std::path::PathBuf::from(
        std::env::var_os("RUSHLS_GAP_LIVE_READY").ok_or("set RUSHLS_GAP_LIVE_READY")?,
    );
    let encoded = encoded_fixture()?;
    std::fs::write(ready.with_extension("ts"), &encoded)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut config = NodeConfig::default();
    config.session.segmentation = crate::segment::SegmentationPolicy::latency_first(
        Duration::from_secs(2),
        Duration::from_millis(200),
    );
    let session = config.session;
    let events = Arc::new(FixtureEvents::default());
    let node = Node::new(
        config,
        Arc::new(OpenStreamAuthenticator::new(StreamPolicy::permissive())),
        Events::new(events.clone()),
        None,
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve(
        crate::server::http::TcpHttpListener::from(listener),
        node.application(),
        HttpConfig::default(),
        None,
        None,
        Readiness::ready(),
        async {
            let _ = stopped.await;
        },
    ));
    std::fs::write(
        &ready,
        format!("http://{address}/live/gap-probe/index.m3u8"),
    )?;
    // The browser driver explicitly starts each case after it is ready.
    let start = ready.with_extension("start");
    tokio::time::timeout(Duration::from_secs(60), async {
        while !start.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    let services = node.services().clone();
    let result = super::run_session(
        Box::new(Publish(encoded)),
        &services,
        &session,
        super::PendingPermit::unlimited(),
    )
    .await;
    std::fs::write(ready.with_extension("outcome"), format!("{result:?}"))?;
    std::fs::write(
        ready.with_extension("events"),
        format!("{:#?}", events.0.lock()),
    )?;
    if result.is_ok() {
        verify_video_notices(&events, &services);
    }
    let done = ready.with_extension("done");
    let _ = tokio::time::timeout(Duration::from_secs(120), async {
        while !done.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let _ = stop.send(());
    server.await??;
    assert!(
        matches!(result, Ok(super::SessionOutcome::Ended)),
        "{result:?}"
    );
    Ok(())
}

fn verify_video_notices(events: &FixtureEvents, services: &super::Services) {
    if let Ok(scenario) = std::env::var("RUSHLS_GAP_LIVE_VIDEO")
        && std::env::var_os("RUSHLS_GAP_LIVE_CONTROL").is_none()
    {
        let events = events.0.lock();
        let notices: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                crate::observe::SessionEvent::Compensation { notice }
                    if notice.status.media_kind == MediaKind::Video =>
                {
                    Some(notice)
                }
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), if scenario == "single" { 2 } else { 6 });
        assert_eq!(
            notices
                .iter()
                .filter(|n| n.transition == crate::domain::RecoveryTransition::Degraded)
                .count(),
            2
        );
        assert!(
            notices
                .iter()
                .all(|n| n.status.method == crate::domain::RecoveryMethod::Gap
                    && n.status.replacement_ticks == 0)
        );
        let counters = services.meters.video_compensation();
        assert_eq!(counters.len(), 1);
        assert_eq!(counters[0].0, ("h264".to_owned(), "gap".to_owned()));
        assert_eq!(counters[0].1.0, notices.len() as u64);
        assert!((counters[0].1.1 - if scenario == "single" { 0.08 } else { 0.24 }).abs() < 1e-9);
    }
}

fn encoded_fixture() -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let video_only = std::env::var_os("RUSHLS_GAP_LIVE_VIDEO_ONLY").is_some();
    let mut command = std::process::Command::new("ffmpeg");
    command.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=25:duration=24",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=24",
        "-map",
        "0:v",
        "-map",
        "0:v",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-g",
        "50",
        "-bf",
        "0",
        "-x264-params",
        "force-cfr=1:scenecut=0",
        "-s:v:1",
        "160x90",
        "-c:a",
        "aac",
        "-b:a",
        "64000",
        "-metadata:s:a:0",
        "language=eng",
        "-metadata:s:a:1",
        "language=swe",
    ]);
    if !video_only {
        command.args(["-map", "1:a", "-map", "1:a"]);
    }
    let encoded = command.args(["-f", "mpegts", "-"]).output()?;
    assert!(
        encoded.status.success(),
        "{}",
        String::from_utf8_lossy(&encoded.stderr)
    );
    Ok(encoded.stdout)
}

#[tokio::test(start_paused = true)]
async fn cancelling_fixture_pacing_keeps_the_pending_packet() -> Result<(), SourceError> {
    for rate in [48_000_u32, 90_000] {
        let meters = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let packet = Packet {
            track_id: TrackId(1),
            pts: Some(i64::from(rate)),
            dts: Some(i64::from(rate)),
            duration: Some(3_600),
            random_access: false,
            audio_trim: crate::domain::AudioTrim::default(),
            webvtt: crate::domain::WebVttCueMetadata::default(),
            subtitle_position: None,
            payload: crate::domain::Payload::from_bytes(bytes::Bytes::from_static(b"packet")),
        };
        let started = tokio::time::Instant::now();
        let mut source = Holes {
            source: MpegTsPacketSource::new(
                Box::new(ReadInput::closed(Cursor::new(Vec::new()))),
                MpegTsConfig::default(),
                InputLimits::permissive(),
                meters.source_view(),
            )?,
            audio: BTreeMap::new(),
            video: BTreeMap::new(),
            video_scenario: None,
            pending: VecDeque::from([packet.clone()]),
            clocks: BTreeMap::from([(
                TrackId(1),
                crate::domain::Timebase::new(
                    nz::u32!(1),
                    std::num::NonZeroU32::new(rate).expect("nonzero clock"),
                ),
            )]),
            state: InputState::Closed,
            anchor: Some((0, started)),
            gaps: false,
        };
        let mut output = Vec::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), source.fill(&mut output))
                .await
                .is_err()
        );
        assert!(output.is_empty());
        assert_eq!(source.pending.front(), Some(&packet));
        assert_eq!(source.fill(&mut output).await?, InputState::Closed);
        assert_eq!(output, vec![packet]);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "the track clock must pace one full second"
        );
    }
    Ok(())
}
