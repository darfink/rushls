//! Tunables for the record e2e. Everything comes from env so the same test
//! serves a 60s local loop and a 600s nightly without code changes.

/// Parsed WxH video size.
#[derive(Clone, Copy, Debug)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.w, self.h)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct E2eConfig {
    /// Synthetic media duration in seconds. 600 = your proposed 10 minutes.
    pub duration_secs: u64,
    pub fps: u32,
    pub size: Size,
    /// Nominal HLS segment length in seconds. 6 is the shipped cadence; 2
    /// keeps every other assertion identical while letting each session be
    /// only ~3s, so the churn test can run many rapid reconnects in a short
    /// local loop.
    pub segment_secs: u64,
    /// GOP length in frames. The IDR interval always divides the segment
    /// target so every segment boundary has a legal cut.
    pub gop_frames: u32,
    pub audio_hz: u32,
    /// How many back-to-back sessions the churn test publishes. The total
    /// synthetic duration is split across this many separately encoded parts.
    pub churn_count: u64,
}

impl E2eConfig {
    pub fn from_env() -> Self {
        let duration_secs = env_u64("RUSHLS_TEST_RECORD_E2E_SECS", 600).clamp(10, 3600);
        let fps = env_u64("RUSHLS_TEST_RECORD_E2E_FPS", 30).clamp(15, 60) as u32;
        let size = env_size("RUSHLS_TEST_RECORD_E2E_SIZE", Size { w: 1280, h: 720 });
        let segment_secs = env_u64("RUSHLS_TEST_RECORD_E2E_SEGMENT_SECS", 6).clamp(1, 6);
        // The IDR interval must divide the segment target or preroll has no
        // legal cut at the nominal boundary: even targets keep the usual 2s
        // GOP, odd targets drop to a 1s GOP which divides any whole-second
        // target.
        let gop_secs = if segment_secs % 2 == 0 { 2 } else { 1 };
        let gop_frames = fps * gop_secs as u32;
        let churn_count = env_u64("RUSHLS_TEST_RECORD_E2E_CHURN_COUNT", 6).clamp(2, 32);
        Self { duration_secs, fps, size, segment_secs, gop_frames, audio_hz: 440, churn_count }
    }

    /// Session cadence for the rig: prod 6s/1s by default, or the short
    /// target above. The short shape mirrors server/http/tests.rs, which
    /// pairs 2s segments with 500ms parts.
    pub fn segmentation_policy(&self) -> rushls::segment::SegmentationPolicy {
        use std::time::Duration;
        let segment = Duration::from_secs(self.segment_secs);
        let part = if self.segment_secs >= 4 { Duration::from_secs(1) } else { Duration::from_millis(500) };
        rushls::segment::SegmentationPolicy::latency_first(segment, part)
    }

    /// Minimum media per session: preroll must observe one full segment
    /// boundary, plus about a second of margin so the closing IDR is always
    /// in frame. With 2s segments this is 3s, which is what lets the churn
    /// test publish many small sessions back to back.
    pub fn min_secs_per_part(&self) -> u64 {
        self.segment_secs + 1
    }

    pub fn expected_video_frames(&self) -> u64 {
        self.duration_secs * u64::from(self.fps)
    }

    pub fn segment_expectation(&self) -> String {
        format!("~{} x {}s segments", self.duration_secs.div_ceil(self.segment_secs), self.segment_secs)
    }
}

fn env_u64(key: &str, fallback: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

fn env_size(key: &str, fallback: Size) -> Size {
    let raw = std::env::var(key).unwrap_or_default();
    let mut parts = raw.split('x');
    let w = parts.next().and_then(|s| s.parse().ok()).unwrap_or(fallback.w);
    let h = parts.next().and_then(|s| s.parse().ok()).unwrap_or(fallback.h);
    Size { w, h }
}
