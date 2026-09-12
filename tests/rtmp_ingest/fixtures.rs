use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use bytes::Bytes;
use rtmpx::ElementaryUnit;
use rushls::{
    admission::{Principal, PublishGrant, StreamPolicy},
    domain::{MediaKind, StreamId},
    observe::{ProcessMeters, SessionMeters},
    source::{
        DiscoveryLimits, IngressEvent, InputState, PendingPublish,
        transport::rtmp::{RtmpConfig, RtmpPendingPublish},
    },
};
use tokio::net::TcpListener;

// Share the checked-in FLV parser with the HLS tests; its other transformations
// are specific to those tests.
#[allow(dead_code)]
#[path = "../apple_hls/flv.rs"]
mod flv;

/// Kill the publisher on failures too, so a stalled test cannot leave a process.
struct Publisher(Child);
impl Drop for Publisher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[allow(clippy::too_many_lines)] // Keep publisher, admission, and byte-equality assertions together.
pub async fn publish_and_compare(name: &str) -> super::TestResult {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/apple_hls/fixtures")
        .join(name);
    let mut expected_audio = Vec::new();
    let mut expected_video = Vec::new();
    for event in flv::ingress_events(&std::fs::read(&fixture)?)? {
        match event {
            IngressEvent::Audio { media, .. } => media.visit_elementary_units(|unit| {
                retain_sample(unit, &mut expected_audio);
            })?,
            IngressEvent::Video { media, .. } => media.visit_elementary_units(|unit| {
                retain_sample(unit, &mut expected_video);
            })?,
            _ => {}
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("rtmp://{}/live/camera", listener.local_addr()?);
    let mut publisher = Publisher(
        Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&fixture)
            .args(["-map", "0:v:0", "-map", "0:a:0", "-c", "copy", "-f", "flv"])
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
    );

    tokio::time::timeout(Duration::from_secs(30), async {
        let (socket, _) = listener.accept().await?;
        let pending = RtmpPendingPublish::handshake_tcp(socket, RtmpConfig::default()).await?;
        let request = pending.publish_request()?;
        assert_eq!(request.resource.namespace.as_deref(), Some("live"));
        assert_eq!(request.resource.name, "camera");
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut accepted = Box::new(pending)
            .accept(
                PublishGrant {
                    stream_id: StreamId::new("live/camera"),
                    principal: Principal("test-publisher".into()),
                    policy: StreamPolicy::permissive(),
                },
                meters.source_view(),
            )
            .await?;
        let discovery = accepted
            .source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 2 * 1024 * 1024,
                maximum_wall_time: Duration::from_secs(10),
            })
            .await?;
        let tracks = discovery.tracks.tracks();
        let audio = tracks
            .iter()
            .find(|t| t.kind() == MediaKind::Audio)
            .ok_or("missing audio")?
            .id;
        let video = tracks
            .iter()
            .find(|t| t.kind() == MediaKind::Video)
            .ok_or("missing video")?
            .id;
        let mut actual_audio = Vec::new();
        let mut actual_video = Vec::new();
        loop {
            let mut packets = Vec::new();
            let state = accepted.source.fill(&mut packets).await?;
            for packet in packets {
                let payload = packet.payload.as_bytes().to_vec();
                if packet.track_id == audio {
                    actual_audio.push(payload);
                } else if packet.track_id == video {
                    actual_video.push(payload);
                } else {
                    return Err("unexpected track".into());
                }
            }
            if !state.is_open() {
                assert_eq!(state, InputState::Closed);
                break;
            }
        }
        assert!(!expected_audio.is_empty() && !expected_video.is_empty());
        assert_eq!(
            actual_audio, expected_audio,
            "audio samples changed across RTMP"
        );
        assert_eq!(
            actual_video, expected_video,
            "video samples changed across RTMP"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    let status = publisher.0.wait()?;
    assert!(status.success(), "ffmpeg failed: {status}");
    Ok(())
}

fn retain_sample(unit: ElementaryUnit, output: &mut Vec<Bytes>) {
    if let ElementaryUnit::Sample { payload, .. } = unit {
        output.push(payload);
    }
}
