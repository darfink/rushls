//! Synthetic source generation via ffmpeg lavfi.
//!
//! testsrc2 carries a frame counter so a dropped/duplicated frame is visible
//! beyond a bare count; sine gives a continuous tone so a cut shows as a gap.
//! H.264 + AAC is the canonical pair: it exercises video random-access,
//! audio priming, and per-rendition record files in one run.

use std::path::PathBuf;
use std::process::Command;

use super::config::E2eConfig;
use super::harness::WorkDir;

/// Names of binaries we shell out to, or a skip reason.
pub fn missing_tools() -> Option<String> {
    for tool in ["ffmpeg", "ffprobe"] {
        if Command::new(tool).arg("-version").output().is_err() {
            return Some(format!("{tool} is not on PATH"));
        }
    }
    None
}

/// Generate one MPEG-TS file with deterministic A+V and return its path.
///
/// GOP is forced (no scene-cut IDRs) so every segment boundary has a legal
/// cut: the IDR interval in the config always divides the segment target.
/// ultrafast keeps a 10-minute 720p30 encode to roughly a minute on CI;
/// use 640x360 locally for an even faster loop.
pub fn generate_mpegts(
    cfg: &E2eConfig,
    work: &WorkDir,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    generate_mpegts_named(cfg, work, "src.ts", cfg.duration_secs)
}

/// Same synthesis under a chosen filename and duration. The reconnect test
/// uses this twice so each half starts on an IDR exactly like a real
/// reconnecting encoder would, rather than by slicing one file mid-GOP.
pub fn generate_mpegts_named(
    cfg: &E2eConfig,
    work: &WorkDir,
    filename: &str,
    secs: u64,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let out = work.path().join(filename);
    let video_src = format!(
        "testsrc2=size={}x{}:rate={}:duration={}",
        cfg.size.w, cfg.size.h, cfg.fps, secs
    );
    let audio_src = format!(
        "sine=frequency={}:sample_rate=48000:duration={}",
        cfg.audio_hz, secs
    );
    let gop = cfg.gop_frames.to_string();
    let fps = cfg.fps.to_string();

    // NOTE: -t is set both per-input (lavfi length) and on the output so an
    // ffmpeg version difference cannot leave audio running past video.
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &video_src,
            "-f",
            "lavfi",
            "-i",
            &audio_src,
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-r",
            &fps,
            "-g",
            &gop,
            "-keyint_min",
            &gop,
            "-x264-params",
            "scenecut=0",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-t",
            &secs.to_string(),
            "-f",
            "mpegts",
        ])
        .arg(&out)
        .status()?;
    if !status.success() {
        return Err(format!("ffmpeg failed to synthesize {}", out.display()).into());
    }
    let bytes = std::fs::metadata(&out)?.len();
    eprintln!(
        "record e2e: synthesized {} ({:.1} MiB)",
        out.display(),
        bytes as f64 / 1_048_576.0
    );
    Ok(out)
}

/// (Burst ingest reads the file in harness.rs; kept here to show intent.)
#[allow(dead_code)]
pub fn read_bytes(
    path: &std::path::Path,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(std::fs::read(path)?)
}
