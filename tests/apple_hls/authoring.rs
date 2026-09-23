//! A two-hour live audit published over Enhanced RTMP, including script captions.
use crate::{
    TestResult,
    harness::{Origin, Setup},
    report::Expect,
};
use rushls::{
    segment::SegmentationPolicy,
    source::transport::rtmp::{RtmpConfig, RtmpPendingPublish},
};
use std::time::Duration;
mod fixture;
const PREFILL_SECONDS: u32 = 7_212;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Apple tools and RUSHLS_TEST_AUTHORING_FIXTURE; run tools/check-apple-authoring.sh"]
async fn two_hour_live_authoring() -> TestResult {
    if !crate::validator::tools_available() {
        return Err("mediastreamvalidator and hlsreport must be installed".into());
    }
    let directory = std::env::var_os("RUSHLS_TEST_AUTHORING_FIXTURE")
        .ok_or("run tools/check-apple-authoring.sh to generate the synthetic fixture")?;
    let tracks = fixture::load(std::path::Path::new(&directory))?;
    let setup = Setup {
        segmentation: SegmentationPolicy::latency_first(
            Duration::from_secs(6),
            Duration::from_secs(1),
        ),
        retain: Some(Duration::from_mins(121)),
        expect: Expect {
            name: "two_hour_live_authoring",
            full_authoring: true,
            captions: true,
            ladder: true,
            unconventional_cadence: false,
            ..Expect::default()
        },
        ..Setup::default()
    };
    let origin = Origin::start_with(setup)
        .await?
        .ok_or("Apple origin did not start")?;
    let (ready, prefilling) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut publisher = AbortOnDrop(tokio::spawn(fixture::publish(
        listener.local_addr()?,
        tracks,
        PREFILL_SECONDS * 1000,
        ready,
    )));
    let (socket, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept()).await??;
    let pending = RtmpPendingPublish::handshake_tcp(socket, RtmpConfig::default()).await?;
    let mut session = AbortOnDrop(origin.spawn_session(Box::new(pending)));
    let result = async {
        tokio::time::timeout(Duration::from_mins(20), async {
            tokio::select! {
                result = prefilling => result.map_err(|error| error.to_string()),
                result = &mut publisher.0 => Err(format!("RTMP publisher stopped before prefill: {result:?}")),
                result = &mut session.0 => Err(format!("origin stopped before prefill: {result:?}")),
            }
        }).await??;
        // The sender can finish its burst while the origin still has queued RTMP
        // packets. Wait for publication, not merely successful socket writes.
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let playlists = origin.media_playlists()?;
                if playlists.len() == fixture::VIDEO_COUNT + fixture::AUDIO_COUNT + 1
                    && playlists.iter().all(|(_, playlist)| {
                        playlist.lines()
                            .filter_map(|line| line.strip_prefix("#EXTINF:"))
                            .filter_map(|line| line.split(',').next()?.parse::<f64>().ok())
                            .sum::<f64>() >= 7200.0
                    })
                {
                    break Ok::<(), Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }).await??;
        verify_presentation(&origin)?;
        let validation = origin.validate_playlist();
        let failures = origin.reported_failures();
        if !failures.is_empty() {
            return Err(format!(
                "publication failed during validation: {failures:?}; {validation:?}"
            )
            .into());
        }
        validation?;
        Ok(())
    }
    .await;
    publisher.0.abort();
    session.0.abort();
    // A select branch may already have consumed a completed task's result.
    if !publisher.0.is_finished() {
        let _ = (&mut publisher.0).await;
    }
    if !session.0.is_finished() {
        let _ = (&mut session.0).await;
    }
    result
}

/// Also close network publishers when setup or an assertion fails.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Check topology and actual caption bytes as well as the advertised DVR window.
fn verify_presentation(origin: &Origin) -> TestResult {
    let master = crate::validator::fetch(origin.url(), origin.certificate_authority())?;
    assert_eq!(
        master
            .lines()
            .filter(|line| line.starts_with("#EXT-X-STREAM-INF:") && line.contains("RESOLUTION="))
            .count(),
        fixture::VIDEO_COUNT
    );
    assert_eq!(
        master
            .lines()
            .filter(|line| line.starts_with("#EXT-X-MEDIA:TYPE=AUDIO,"))
            .count(),
        fixture::AUDIO_COUNT
    );
    let subtitle = master
        .lines()
        .find(|line| line.starts_with("#EXT-X-MEDIA:TYPE=SUBTITLES,"))
        .ok_or("missing subtitle rendition")?;
    assert!(subtitle.contains("LANGUAGE=\"en\""));
    save_artifact("m3u8", &master)?;
    let playlists = origin.media_playlists()?;
    assert_eq!(
        playlists.len(),
        fixture::VIDEO_COUNT + fixture::AUDIO_COUNT + 1
    );
    let mut windows = Vec::new();
    for (index, (url, playlist)) in playlists.iter().enumerate() {
        assert!(
            !playlist.contains("#EXT-X-ENDLIST"),
            "expected live media: {url}"
        );
        let seconds: f64 = playlist
            .lines()
            .filter_map(|line| line.strip_prefix("#EXTINF:"))
            .map(|line| line.split(',').next().unwrap_or_default().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .sum();
        assert!(seconds >= 7200.0, "{url} advertises only {seconds}s of DVR");
        save_artifact(&format!("{index}.m3u8"), playlist)?;
        windows.push(serde_json::json!({"url": url, "seconds": seconds}));
        if url.contains("subtitles.m3u8") {
            let text = crate::validator::fetch_webvtt(playlist, origin.certificate_authority())?;
            assert!(text.starts_with("WEBVTT"));
            assert!(
                text.contains("Synthetic caption at "),
                "onCaption text missing from retained WebVTT"
            );
            save_artifact("vtt", &text)?;
        }
    }
    save_artifact("windows.json", &serde_json::to_string_pretty(&windows)?)?;
    Ok(())
}

fn save_artifact(extension: &str, text: &str) -> TestResult {
    if let Some(directory) = std::env::var_os("RUSHLS_TEST_REPORT_DIR") {
        std::fs::write(
            std::path::Path::new(&directory).join(format!("two_hour_live_authoring.{extension}")),
            text,
        )?;
    }
    Ok(())
}
