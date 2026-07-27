use std::{collections::VecDeque, time::Duration};

use super::RenditionBitrateStatistics;

const AVERAGE_WINDOW: Duration = Duration::from_hours(1);

#[derive(Clone, Copy, Debug)]
struct Observation {
    bits: u128,
    duration: Duration,
}

/// Incremental HLS segment-bitrate statistics for one rendition.
///
/// The peak is monotonic over the stream lifetime and considers exactly the
/// contiguous duration ranges defined by HLS. The average is intentionally
/// bounded to approximately the latest media hour so old operating conditions
/// do not dominate a long-running live stream.
#[derive(Debug, Default)]
pub(super) struct BitrateTracker {
    peak_bits_per_second: Option<u64>,
    peak_window: VecDeque<Observation>,
    peak_window_duration: Duration,
    average_window: VecDeque<Observation>,
    average_window_duration: Duration,
    average_window_bits: u128,
    observed_segments: u64,
}

impl BitrateTracker {
    pub(super) fn break_contiguity(&mut self) {
        self.peak_window.clear();
        self.peak_window_duration = Duration::ZERO;
    }

    pub(super) fn observe(&mut self, bytes: usize, duration: Duration, target: Duration) {
        if duration.is_zero() {
            return;
        }
        let observation = Observation {
            bits: (bytes as u128).saturating_mul(8),
            duration,
        };
        self.observed_segments = self.observed_segments.saturating_add(1);

        self.peak_window.push_back(observation);
        self.peak_window_duration = self.peak_window_duration.saturating_add(duration);
        let lower = duration_from_nanos(target.as_nanos() / 2);
        let upper = duration_from_nanos(
            target
                .as_nanos()
                .saturating_mul(3)
                .saturating_div(2)
                .saturating_add(Duration::from_millis(500).as_nanos()),
        );
        let mut suffix_duration = Duration::ZERO;
        let mut suffix_bits = 0_u128;
        for candidate in self.peak_window.iter().rev() {
            suffix_duration = suffix_duration.saturating_add(candidate.duration);
            suffix_bits = suffix_bits.saturating_add(candidate.bits);
            if suffix_duration > upper {
                break;
            }
            if suffix_duration >= lower {
                let rate = bits_per_second(suffix_bits, suffix_duration);
                self.peak_bits_per_second = Some(self.peak_bits_per_second.unwrap_or(0).max(rate));
            }
        }
        while self
            .peak_window
            .front()
            .is_some_and(|front| self.peak_window_duration.saturating_sub(front.duration) >= upper)
        {
            let removed = self.peak_window.pop_front().expect("front exists");
            self.peak_window_duration = self.peak_window_duration.saturating_sub(removed.duration);
        }

        self.average_window.push_back(observation);
        self.average_window_duration = self
            .average_window_duration
            .saturating_add(observation.duration);
        self.average_window_bits = self.average_window_bits.saturating_add(observation.bits);
        while self.average_window.front().is_some_and(|front| {
            self.average_window_duration.saturating_sub(front.duration) >= AVERAGE_WINDOW
        }) {
            let removed = self.average_window.pop_front().expect("front exists");
            self.average_window_duration = self
                .average_window_duration
                .saturating_sub(removed.duration);
            self.average_window_bits = self.average_window_bits.saturating_sub(removed.bits);
        }
    }

    pub(super) fn snapshot(&self) -> RenditionBitrateStatistics {
        RenditionBitrateStatistics {
            peak_bits_per_second: self.peak_bits_per_second,
            average_bits_per_second: (!self.average_window_duration.is_zero())
                .then(|| bits_per_second(self.average_window_bits, self.average_window_duration)),
            observed_segments: self.observed_segments,
        }
    }
}

fn bits_per_second(bits: u128, duration: Duration) -> u64 {
    if duration.is_zero() {
        return 0;
    }
    bits.saturating_mul(1_000_000_000)
        .checked_div(duration.as_nanos())
        .unwrap_or(u128::from(u64::MAX))
        .min(u128::from(u64::MAX)) as u64
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}
