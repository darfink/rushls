//! Malformed-input tests: garbage must never poison the archive.
//!
//! Rejecting junk is a fast in-process assertion, so the rejection cases run
//! in the default suite with no ffmpeg. The truncated-prefix case needs
//! synthesis, so it stays ignored like the other end-to-end tests.

use std::collections::BTreeMap;

use rushls::session::{SessionError, SessionOutcome};

use super::config::E2eConfig;
use super::harness::{DrainedArchive, TestRig, WorkDir};
use super::verify;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Publish bytes that are not media at all and require a clean rejection
/// with an empty archive afterwards. Each case gets a fresh rig so one
/// poisoned session cannot affect the next.
pub async fn expect_rejected(bytes: Vec<u8>, label: &str) -> TestResult {
    let cfg = E2eConfig::from_env();
    let work = WorkDir::new(&cfg)?;
    let rig = TestRig::start(work.archive(), "{rendition}_{segment}.mp4", &cfg)?;
    match rig.run_burst(bytes).await {
        Ok(outcome) => return Err(format!("{label}: expected rejection, got {outcome:?}").into()),
        Err(error) => eprintln!("record e2e malformed {label}: rejected as expected ({error:?})"),
    }
    let drained = rig.drain_and_collect().await?;
    if !drained.files.is_empty() {
        return Err(format!(
            "{label}: archive holds {} files after rejected input",
            drained.files.len()
        )
        .into());
    }
    if drained.recording_lost != 0 {
        return Err(format!(
            "{label}: recorder lost {} segments on rejected input",
            drained.recording_lost
        )
        .into());
    }
    Ok(())
}

/// A file cut mid-stream is a valid prefix, not garbage: whatever was
/// complete before the cut must be contiguous from zero, decode clean, and
/// stay within the full-length totals. Any outcome is accepted because an
/// abrupt EOF is modelled as an early close; what matters is that the
/// archive holds a clean prefix and nothing torn.
pub fn check_truncated_prefix(
    cfg: &E2eConfig,
    work: &WorkDir,
    result: &Result<SessionOutcome, SessionError>,
    drained: &DrainedArchive,
) -> TestResult {
    match result {
        Ok(outcome) => eprintln!("record e2e truncated: session {outcome:?}"),
        Err(error) => {
            eprintln!(
                "record e2e truncated: session errored ({error:?}), checking partial archive"
            );
        }
    }
    for event in &drained.node_events {
        let msg = format!("{event:?}");
        if msg.contains("RecordingFailed") || msg.contains("DrainExpired") {
            return Err(format!("recorder reported failure: {msg}").into());
        }
    }
    if drained.recording_lost != 0 {
        return Err(format!("recorder lost {} segments", drained.recording_lost).into());
    }
    if drained.files.is_empty() {
        return Err("archive is empty after a 60 percent prefix of valid media".into());
    }
    verify::check_no_temp_files(work.archive())?;

    let mut by_rendition: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for file in &drained.files {
        let (key, seg) = verify::split_key(work.archive(), file)
            .ok_or_else(|| format!("unexpected archive name: {}", file.display()))?;
        by_rendition.entry(key).or_default().push(seg);
    }
    for (rendition, segs) in &mut by_rendition {
        segs.sort_unstable();
        for (i, seg) in segs.iter().enumerate() {
            if *seg != i as u64 {
                return Err(format!(
                    "rendition {rendition}: gap, expected segment {i}, found {seg}"
                )
                .into());
            }
        }
    }

    let mut video_frames: u64 = 0;
    let mut audio_frames: u64 = 0;
    let mut audio_sr: u32 = 0;
    for file in &drained.files {
        let info = verify::probe(file)?;
        if info.has_video {
            video_frames += info.video_frames;
        }
        if info.has_audio {
            audio_frames += info.audio_frames;
            if audio_sr == 0 {
                audio_sr = info.sample_rate;
            }
        }
        verify::decode_clean(file)?;
    }

    // Prefix totals must stay at or below the full-length expectations: a
    // truncated ingest cannot record more than the whole source would.
    let expected_video = cfg.expected_video_frames();
    let slack = u64::from(cfg.gop_frames);
    eprintln!("record e2e truncated: video frames {video_frames} vs full-length {expected_video}");
    if video_frames == 0 {
        return Err("no video frames recorded from a 60 percent prefix".into());
    }
    if video_frames > expected_video + slack {
        return Err(
            format!("video frame total {video_frames} above full-length {expected_video}").into(),
        );
    }
    if audio_frames > 0 || audio_sr > 0 {
        let sr = if audio_sr > 0 { audio_sr } else { 48_000 };
        let expected_audio = (u64::from(sr) * cfg.duration_secs).div_ceil(1024);
        eprintln!(
            "record e2e truncated: audio frames {audio_frames} vs full-length {expected_audio}"
        );
        if audio_frames > expected_audio + 8 {
            return Err(format!(
                "audio frame total {audio_frames} above full-length {expected_audio}"
            )
            .into());
        }
    }
    eprintln!(
        "record e2e truncated: PASS ({} files, {} renditions)",
        drained.files.len(),
        by_rendition.len()
    );
    Ok(())
}
