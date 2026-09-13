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
    /// GOP length in frames. 2s IDR interval divides the 6s prod segment.
    pub gop_frames: u32,
    pub audio_hz: u32,
}

impl E2eConfig {
    pub fn from_env() -> Self {
        let duration_secs = env_u64("RUSHLS_TEST_RECORD_E2E_SECS", 600).clamp(10, 3600);
        let fps = env_u64("RUSHLS_TEST_RECORD_E2E_FPS", 30).clamp(15, 60) as u32;
        let size = env_size("RUSHLS_TEST_RECORD_E2E_SIZE", Size { w: 1280, h: 720 });
        // Keep the IDR interval at 2s regardless of fps so cuts stay aligned
        // with the default 6s segment target.
        let gop_frames = fps * 2;
        Self { duration_secs, fps, size, gop_frames, audio_hz: 440 }
    }

    pub fn expected_video_frames(&self) -> u64 {
        self.duration_secs * u64::from(self.fps)
    }

    pub fn segment_expectation(&self) -> String {
        // Prod cadence is 6s segments; burst ingest should yield ~duration/6.
        format!("~{} x 6s segments", self.duration_secs.div_ceil(6))
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
