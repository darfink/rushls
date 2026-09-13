//! Reconnect-grace variant of the burst record test.
//!
//! Two separately generated inputs, two sequential sessions, one shared rig
//! and archive. Each half starts on an IDR like a real reconnecting encoder,
//! so every segment boundary has a legal cut without slicing one file mid-GOP.
//! The archive pattern carries the publication id, which the server mints
//! fresh per accepted session, so the restarted segment numbers of the second
//! half land next to the first half instead of on top of it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rushls::session::SessionOutcome;

use super::config::E2eConfig;
use super::harness::{RecordOutcome, TestRig, WorkDir};
use super::verify;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[allow(dead_code)]
pub struct ReconnectOutcome {
    pub outcome: RecordOutcome,
    pub first: SessionOutcome,
    pub second: SessionOutcome,
    pub secs_a: u64,
    pub secs_b: u64,
}

/// Publish both halves back to back into one archive.
pub async fn publish_two_halves(
    work: &WorkDir,
    ts_a: &Path,
    ts_b: &Path,
    secs_a: u64,
    secs_b: u64,
) -> Result<ReconnectOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let bytes_a = std::fs::read(ts_a)?;
    let bytes_b = std::fs::read(ts_b)?;
    let rig = TestRig::start(work.archive(), "{publication}/{rendition}_{segment}.mp4")?;
    let first = rig.run_burst(bytes_a).await?;
    if first != SessionOutcome::Ended {
        return Err(format!("first burst session did not end cleanly: {first:?}").into());
    }
    let second = rig.run_burst(bytes_b).await?;
    if second != SessionOutcome::Ended {
        return Err(format!("second burst session did not end cleanly: {second:?}").into());
    }
    let drained = rig.drain_and_collect().await?;
    let outcome = RecordOutcome {
        session: second,
        node_events: drained.node_events,
        recording_lost: drained.recording_lost,
        files: drained.files,
    };
    Ok(ReconnectOutcome { outcome, first, second, secs_a, secs_b })
}

/// The archive must hold both halves whole: exactly two publication
/// prefixes, contiguous segments from zero under each, and combined plus
/// per-publication frame totals matching the two synthetic sources.
pub fn verify_reconnect(cfg: &E2eConfig, work: &WorkDir, r: &ReconnectOutcome) -> TestResult {
    let outcome = &r.outcome;
    for event in &outcome.node_events {
        let msg = format!("{event:?}");
        if msg.contains("RecordingFailed") || msg.contains("DrainExpired") {
            return Err(format!("recorder reported failure: {msg}").into());
        }
    }
    if outcome.recording_lost != 0 {
        return Err(format!("recorder lost {} segments", outcome.recording_lost).into());
    }
    if outcome.files.is_empty() {
        return Err("archive is empty after two successful sessions".into());
    }
    verify::check_no_temp_files(work.archive())?;

    // Group by rendition key; the publication id is the first path component.
    let mut by_rendition: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut publications: BTreeSet<String> = BTreeSet::new();
    for file in &outcome.files {
        let (key, seg) = verify::split_key(work.archive(), file)
            .ok_or_else(|| format!("unexpected archive name: {}", file.display()))?;
        let publication = key.split("/").next().unwrap_or("").to_string();
        publications.insert(publication);
        by_rendition.entry(key).or_default().push(seg);
    }
    if publications.len() != 2 {
        return Err(format!("expected exactly 2 publication prefixes, found {}: {publications:?}", publications.len()).into());
    }
    for (rendition, segs) in &mut by_rendition {
        segs.sort_unstable();
        for (i, seg) in segs.iter().enumerate() {
            if *seg != i as u64 {
                return Err(format!("rendition {rendition}: gap, expected segment {i}, found {seg}").into());
            }
        }
        eprintln!("record e2e reconnect: rendition {rendition}: {} segments", segs.len());
    }

    // Per-file probe plus full decode, summed overall and per publication.
    let mut video_frames: u64 = 0;
    let mut audio_frames: u64 = 0;
    let mut audio_sr: u32 = 0;
    let mut video_by_pub: BTreeMap<String, u64> = BTreeMap::new();
    for file in &outcome.files {
        let (key, _) = verify::split_key(work.archive(), file)
            .ok_or_else(|| format!("unexpected archive name: {}", file.display()))?;
        let publication = key.split("/").next().unwrap_or("").to_string();
        let info = verify::probe(file)?;
        if info.has_video {
            video_frames += info.video_frames;
            *video_by_pub.entry(publication).or_default() += info.video_frames;
        }
        if info.has_audio {
            audio_frames += info.audio_frames;
            if audio_sr == 0 {
                audio_sr = info.sample_rate;
            }
        }
        verify::decode_clean(file)?;
    }

    // Each half must have survived whole. Both sides are sorted because the
    // two publication ids are random and carry no half order.
    let mut got: Vec<u64> = video_by_pub.values().copied().collect();
    got.sort_unstable();
    let mut want = vec![u64::from(cfg.fps) * r.secs_a, u64::from(cfg.fps) * r.secs_b];
    want.sort_unstable();
    let slack = u64::from(cfg.gop_frames);
    if got.len() != 2 {
        return Err(format!("expected video under 2 publications, found {}", got.len()).into());
    }
    for (index, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        eprintln!("record e2e reconnect: publication {index}: video frames {g} vs expected {w}");
        if *g + slack < *w || *g > *w + slack {
            return Err(format!("publication {index}: video frames {g} far from expected {w}").into());
        }
    }
    eprintln!("record e2e reconnect: video frames {video_frames} total");

    // Audio priming repeats per publication, so the expectation is the sum
    // of the two half ceilings with a wider positive slack than the base test.
    if audio_frames > 0 || audio_sr > 0 {
        let sr = if audio_sr > 0 { audio_sr } else { 48_000 };
        let expected_audio = (u64::from(sr) * r.secs_a).div_ceil(1024) + (u64::from(sr) * r.secs_b).div_ceil(1024);
        eprintln!("record e2e reconnect: audio frames {audio_frames} vs expected {expected_audio}");
        if audio_frames + 2 < expected_audio {
            return Err(format!("audio frame total {audio_frames} below expected {expected_audio}").into());
        }
        if audio_frames > expected_audio + 10 {
            return Err(format!("audio frame total {audio_frames} far above expected {expected_audio}").into());
        }
    } else {
        eprintln!("record e2e reconnect: no audio renditions found; skipping audio total");
    }
    let _ = cfg;
    eprintln!("record e2e reconnect: PASS ({} files, {} renditions, 2 publications)", outcome.files.len(), by_rendition.len());
    Ok(())
}
