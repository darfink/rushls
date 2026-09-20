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
#[allow(dead_code)]
pub struct ChurnOutcome {
    pub outcome: RecordOutcome,
    pub sessions: Vec<SessionOutcome>,
    pub secs: Vec<u64>,
}

/// Publish every file back to back into one archive, one session each. The
/// pattern carries the publication id so the restarted segment numbers of
/// each session land next to, not on top of, the earlier ones.
pub async fn publish_many_halves(
    cfg: &E2eConfig,
    work: &WorkDir,
    files: &[std::path::PathBuf],
    secs: &[u64],
) -> Result<ChurnOutcome, Box<dyn std::error::Error + Send + Sync>> {
    if files.is_empty() || files.len() != secs.len() {
        return Err(format!(
            "churn needs one duration per file, got {} files and {} durations",
            files.len(),
            secs.len()
        )
        .into());
    }
    let rig = TestRig::start(
        work.archive(),
        "{publication}/{rendition}_{segment}.mp4",
        cfg,
    )?;
    let mut sessions = Vec::new();
    for (index, (file, s)) in files.iter().zip(secs.iter()).enumerate() {
        let bytes = std::fs::read(file)?;
        eprintln!("record e2e churn: session {index} publishing {s}s of synthetic input");
        let session = rig.run_burst(bytes).await?;
        if session != SessionOutcome::Ended {
            return Err(format!("burst session {index} did not end cleanly: {session:?}").into());
        }
        sessions.push(session);
    }
    let drained = rig.drain_and_collect().await?;
    let last = sessions
        .last()
        .copied()
        .ok_or("churn published no sessions")?;
    let outcome = RecordOutcome {
        session: last,
        node_events: drained.node_events,
        recording_lost: drained.recording_lost,
        files: drained.files,
    };
    Ok(ChurnOutcome {
        outcome,
        sessions,
        secs: secs.to_vec(),
    })
}

/// Publish both halves back to back into one archive.
pub async fn publish_two_halves(
    cfg: &E2eConfig,
    work: &WorkDir,
    ts_a: &Path,
    ts_b: &Path,
    secs_a: u64,
    secs_b: u64,
) -> Result<ReconnectOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let files = vec![ts_a.to_path_buf(), ts_b.to_path_buf()];
    let c = publish_many_halves(cfg, work, &files, &[secs_a, secs_b]).await?;
    Ok(ReconnectOutcome {
        outcome: c.outcome,
        first: c.sessions[0],
        second: c.sessions[1],
        secs_a,
        secs_b,
    })
}

/// The archive must hold every session whole: exactly one publication prefix
/// per session, contiguous segments from zero under each, and combined plus
/// per-publication frame totals matching the synthetic sources.
pub fn verify_many_halves(
    cfg: &E2eConfig,
    work: &WorkDir,
    outcome: &RecordOutcome,
    secs: &[u64],
) -> TestResult {
    let n = secs.len();
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
        return Err(format!("archive is empty after {n} successful sessions").into());
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
    if publications.len() != n {
        return Err(format!(
            "expected exactly {n} publication prefixes, found {}: {publications:?}",
            publications.len()
        )
        .into());
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
        eprintln!(
            "record e2e churn: rendition {rendition}: {} segments",
            segs.len()
        );
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

    // Every session must have survived whole. Both sides are sorted because
    // the publication ids are random and carry no session order.
    let mut got: Vec<u64> = video_by_pub.values().copied().collect();
    got.sort_unstable();
    let mut want: Vec<u64> = secs.iter().map(|s| u64::from(cfg.fps) * s).collect();
    want.sort_unstable();
    let slack = u64::from(cfg.gop_frames);
    if got.len() != n {
        return Err(format!("expected video under {n} publications, found {}", got.len()).into());
    }
    for (index, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        eprintln!("record e2e churn: publication {index}: video frames {g} vs expected {w}");
        if *g + slack < *w || *g > *w + slack {
            return Err(
                format!("publication {index}: video frames {g} far from expected {w}").into(),
            );
        }
    }
    eprintln!("record e2e churn: video frames {video_frames} total");

    // Audio priming repeats per publication, so the expectation is the sum of
    // the per-session ceilings with positive slack that grows with the count.
    if audio_frames > 0 || audio_sr > 0 {
        let sr = if audio_sr > 0 { audio_sr } else { 48_000 };
        let expected_audio: u64 = secs
            .iter()
            .map(|s| (u64::from(sr) * s).div_ceil(1024))
            .sum();
        eprintln!("record e2e churn: audio frames {audio_frames} vs expected {expected_audio}");
        if audio_frames + 2 < expected_audio {
            return Err(format!(
                "audio frame total {audio_frames} below expected {expected_audio}"
            )
            .into());
        }
        if audio_frames > expected_audio + 4 * n as u64 + 4 {
            return Err(format!(
                "audio frame total {audio_frames} far above expected {expected_audio}"
            )
            .into());
        }
    } else {
        eprintln!("record e2e churn: no audio renditions found; skipping audio total");
    }
    eprintln!(
        "record e2e churn: PASS ({} files, {} renditions, {n} publications)",
        outcome.files.len(),
        by_rendition.len()
    );
    Ok(())
}

/// The archive must hold both halves whole: exactly two publication prefixes.
pub fn verify_reconnect(cfg: &E2eConfig, work: &WorkDir, r: &ReconnectOutcome) -> TestResult {
    let secs = [r.secs_a, r.secs_b];
    verify_many_halves(cfg, work, &r.outcome, &secs)
}
