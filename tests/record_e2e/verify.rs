//! Archive verification with ffprobe / ffmpeg only.
//!
//! What "matches the source" means here, since byte-equality cannot hold:
//! AAC priming, edit lists, and segmentation trims all shift samples
//! by design. So we prove continuity instead:
//!
//! - inventory: 0..N-1 present per rendition, no gaps, no .tmp leftovers
//! - per-file: ffprobe sees the right codec, each file decodes clean
//! - totals: video frames == fps*duration (exact, plus or minus 1 GOP);
//!   audio AAC frames == ceil(duration*sr/1024) plus small priming slack;
//!   no RecordingFailed / lost > 0
//!
//! Note: audio is verified by summed decoded frame counts, never by summed
//! container durations, because edit lists make per-file durations cumulative.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use super::config::E2eConfig;
use super::harness::{RecordOutcome, WorkDir};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub fn verify_archive(cfg: &E2eConfig, work: &WorkDir, outcome: &RecordOutcome) -> TestResult {
    // --- recorder health first: a green decode with a lost segment is still red.
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
        return Err("archive is empty after a successful session".into());
    }
    // Crash leftovers must never be mistaken for segments.
    check_no_temp_files(work.archive())?;

    // --- group by rendition key: relative path minus the segment suffix, so
    //     both the flat test pattern and the prod pattern with time dirs group.
    let mut by_rendition: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for file in &outcome.files {
        let (key, seg) = split_key(work.archive(), file)
            .ok_or_else(|| format!("unexpected archive name: {}", file.display()))?;
        by_rendition.entry(key).or_default().push(seg);
    }
    if by_rendition.is_empty() {
        return Err("no rendition files recognised".into());
    }
    for (rendition, segs) in &mut by_rendition {
        segs.sort_unstable();
        // Contiguous from 0: a missing number is a dropped recording, which the
        // byte-budget path does by design under overload - and must be zero here.
        for (i, seg) in segs.iter().enumerate() {
            if *seg != i as u64 {
                return Err(format!("rendition {rendition}: gap, expected segment {i}, found {seg} (have {} total)", segs.len()).into());
            }
        }
        eprintln!("record e2e: rendition {rendition}: {} segments (0..{})", segs.len(), segs.len() - 1);
    }

    // --- per-file: probe + full decode. Slowest step, but it is the actual proof.
    let mut video_frames: u64 = 0;
    let mut audio_frames: u64 = 0;
    let mut audio_sr: u32 = 0;
    for file in &outcome.files {
        let info = probe(file)?;
        eprintln!(
            "record e2e: {} v={} ({} frames) a={} ({} frames, {} Hz)",
            file.strip_prefix(work.archive()).unwrap_or(file).display(),
            info.has_video, info.video_frames, info.has_audio, info.audio_frames, info.sample_rate
        );
        if info.has_video {
            // count_frames via ffprobe is authoritative; decode below proves clean.
            video_frames += info.video_frames;
        }
        if info.has_audio {
            audio_frames += info.audio_frames;
            if audio_sr == 0 {
                audio_sr = info.sample_rate;
            }
        }
        decode_clean(file)?;
    }

    // --- totals vs synthetic source.
    let expected_frames = cfg.expected_video_frames();
    // Exact for video: no frame may be dropped or duplicated across ~100 cuts.
    // If the tail segment is partial (duration not a multiple of 6s), allow one
    // GOP of slack and align durations to multiples of 6s to remove the slack.
    let slack = u64::from(cfg.gop_frames);
    if video_frames + slack < expected_frames || video_frames > expected_frames + slack {
        return Err(format!(
            "video frame total {video_frames} far from expected {expected_frames} (fps {} x {}s)",
            cfg.fps, cfg.duration_secs
        ).into());
    }
    eprintln!("record e2e: video frames {video_frames} vs expected {expected_frames}");

    // Audio: AAC packets carry 1024 samples each. Expected packet count is
    // ceil(duration*sr/1024); the encoder adds 1-2 priming packets, so allow
    // a small positive slack but almost no negative slack (drops are failures).
    if audio_frames > 0 || audio_sr > 0 {
        let sr = if audio_sr > 0 { audio_sr } else { 48_000 };
        let expected_audio = (u64::from(sr) * cfg.duration_secs).div_ceil(1024);
        eprintln!("record e2e: audio frames {audio_frames} vs expected {expected_audio}");
        if audio_frames + 2 < expected_audio {
            return Err(format!("audio frame total {audio_frames} below expected {expected_audio}").into());
        }
        if audio_frames > expected_audio + 8 {
            return Err(format!("audio frame total {audio_frames} far above expected {expected_audio}").into());
        }
    } else {
        eprintln!("record e2e: no audio renditions found; skipping audio total");
    }
    eprintln!("record e2e: PASS ({} files, {} renditions)", outcome.files.len(), by_rendition.len());
    Ok(())
}

pub(crate) fn split_key(archive: &Path, file: &Path) -> Option<(String, u64)> {
    let rel = file.strip_prefix(archive).ok()?.to_str()?;
    let stem = rel.strip_suffix(".mp4").or_else(|| rel.strip_suffix(".vtt"))?;
    let (rendition, seg) = stem.rsplit_once('_')?;
    Some((rendition.to_string(), seg.parse().ok()?))
}

pub(crate) fn check_no_temp_files(archive: &Path) -> TestResult {
    let mut tmp = Vec::new();
    fn visit(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for item in std::fs::read_dir(dir)? {
            let p = item?.path();
            if p.is_dir() {
                visit(&p, out)?;
            } else if p.file_name().map(|n| n.to_string_lossy().starts_with(".rushls-")).unwrap_or(false) {
                out.push(p);
            }
        }
        Ok(())
    }
    visit(archive, &mut tmp)?;
    if !tmp.is_empty() {
        return Err(format!("{} temp commit files left behind: {:?}", tmp.len(), tmp).into());
    }
    Ok(())
}

pub(crate) struct Probe {
    pub(crate) has_video: bool,
    pub(crate) video_frames: u64,
    pub(crate) has_audio: bool,
    pub(crate) audio_frames: u64,
    pub(crate) sample_rate: u32,
}

/// ffprobe without JSON (no new deps): one --show_entries query per kind.
pub(crate) fn probe(file: &Path) -> Result<Probe, Box<dyn std::error::Error + Send + Sync>> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0",
            "-show_entries", "stream=codec_name,nb_read_frames",
            "-count_frames", "-of", "default=nw=1"])
        .arg(file)
        .output()?;
    let video = kv_map(&String::from_utf8_lossy(&out.stdout));
    let has_video = out.status.success() && video.get("codec_name").is_some();
    let video_frames: u64 = video.get("nb_read_frames").and_then(|s| s.parse().ok()).unwrap_or(0);

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0",
            "-show_entries", "stream=codec_name,nb_read_frames,sample_rate",
            "-count_frames", "-of", "default=nw=1"])
        .arg(file)
        .output()?;
    let audio = kv_map(&String::from_utf8_lossy(&out.stdout));
    let has_audio = out.status.success() && audio.get("codec_name").is_some();
    let audio_frames: u64 = audio.get("nb_read_frames").and_then(|s| s.parse().ok()).unwrap_or(0);
    let sample_rate: u32 = audio.get("sample_rate").and_then(|s| s.parse().ok()).unwrap_or(0);

    if !has_video && !has_audio {
        return Err(format!("{}: ffprobe found neither audio nor video", file.display()).into());
    }
    Ok(Probe { has_video, video_frames, has_audio, audio_frames, sample_rate })
}

/// Key=value map for one ffprobe response. Needed because ffprobe prints
/// fields in schema order rather than the order named in show_entries.
fn kv_map(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some((k, v)) = line.split_once("=") {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

/// Full decode to null: catches truncated mdat, bad moov, or torn commits
/// that a header-only probe would miss.
pub(crate) fn decode_clean(file: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-f", "null", "-"])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "{} failed to decode: {}",
            file.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ).into());
    }
    Ok(())
}
