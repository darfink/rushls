//! Fixed, bounded duration distributions. Owners synchronize updates and snapshots.

use std::time::Duration;

/// Seconds, including sub-part scheduling jitter and long output interruptions.
pub const DURATION_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

#[derive(Clone, Debug, Default)]
pub struct DurationHistogram {
    /// Non-cumulative storage keeps an observation to one increment.
    pub buckets: [u64; 14],
    pub count: u64,
    pub sum: f64,
}

impl DurationHistogram {
    pub fn observe(&mut self, value: Duration) {
        let seconds = value.as_secs_f64();
        self.count += 1;
        self.sum += seconds;
        if let Some(index) = DURATION_BUCKETS.iter().position(|bound| seconds <= *bound) {
            self.buckets[index] += 1;
        }
    }
}
