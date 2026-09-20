//! Shared internal compensation defaults and per-track rolling accounting.
use std::time::Duration;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompensationPolicy {
    pub enabled: bool,
    // Full replacement duration for audio; excess over nominal cadence for video.
    pub maximum_hole: Duration,
    pub maximum_compensation: Duration,
    pub maximum_holes: usize,
    pub window: Duration,
    pub clean_interval: Duration,
}
impl Default for CompensationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            maximum_hole: Duration::from_millis(500),
            maximum_compensation: Duration::from_secs(1),
            maximum_holes: 10,
            window: Duration::from_secs(60),
            clean_interval: Duration::from_secs(30),
        }
    }
}
#[cfg(test)]
impl CompensationPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.maximum_hole.is_zero()
            || self.maximum_compensation.is_zero()
            || self.maximum_holes == 0
            || self.window.is_zero()
            || self.clean_interval.is_zero()
        {
            return Err("compensation limits must be positive");
        }
        Ok(())
    }
}

// Each normalizer owns its budget. Endpoint and duration clocks may differ
// (video uses source timestamps but accounts excess on an exact common clock).
use crate::domain::{RecoveryRejection, Timebase};
use std::collections::VecDeque;

pub struct CompensationBudget {
    pub policy: CompensationPolicy,
    endpoint_clock: Timebase,
    duration_clock: Timebase,
    history: VecDeque<(i64, u64)>,
}
impl CompensationBudget {
    pub fn new(endpoint_clock: Timebase, duration_clock: Timebase) -> Self {
        Self {
            policy: CompensationPolicy::default(),
            endpoint_clock,
            duration_clock,
            history: VecDeque::new(),
        }
    }
    fn scaled(ticks: u64, clock: Timebase) -> u128 {
        u128::from(ticks) * u128::from(clock.num().get()) * 1_000_000_000
    }
    pub fn exceeds(&self, ticks: u64, limit: Duration) -> bool {
        Self::scaled(ticks, self.duration_clock)
            > limit.as_nanos() * u128::from(self.duration_clock.den().get())
    }
    pub fn clean(&self, ticks: u64, clock: Timebase) -> bool {
        Self::scaled(ticks, clock)
            >= self.policy.clean_interval.as_nanos() * u128::from(clock.den().get())
    }
    fn retained(&self, current: i64, previous: i64) -> bool {
        previous <= current
            && Self::scaled(current.abs_diff(previous), self.endpoint_clock)
                < self.policy.window.as_nanos() * u128::from(self.endpoint_clock.den().get())
    }
    /// Validate without mutating history so a rejected repair is transactional.
    pub fn check(&self, end: i64, ticks: u64) -> Result<(), RecoveryRejection> {
        if self.exceeds(ticks, self.policy.maximum_hole) {
            return Err(RecoveryRejection::MaximumHole);
        }
        let mut count = 1usize;
        let mut total = ticks;
        for &(previous, duration) in &self.history {
            if self.retained(end, previous) {
                count = count
                    .checked_add(1)
                    .ok_or(RecoveryRejection::UnrepresentableDuration)?;
                total = total
                    .checked_add(duration)
                    .ok_or(RecoveryRejection::UnrepresentableDuration)?;
            }
        }
        if count > self.policy.maximum_holes {
            return Err(RecoveryRejection::MaximumHoles);
        }
        if self.exceeds(total, self.policy.maximum_compensation) {
            return Err(RecoveryRejection::MaximumCompensation);
        }
        Ok(())
    }
    /// Commit only after all codec and track-specific checks have succeeded.
    pub fn commit(&mut self, end: i64, ticks: u64) {
        while self
            .history
            .front()
            .is_some_and(|&(previous, _)| !self.retained(end, previous))
        {
            self.history.pop_front();
        }
        self.history.push_back((end, ticks));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusive_limits_expiration_and_independent_tracks() {
        let clock = Timebase::new(nz::u32!(1), nz::u32!(1_000));
        let mut audio = CompensationBudget::new(clock, clock);
        let video = CompensationBudget::new(clock, clock);
        assert!(audio.check(500, 500).is_ok());
        audio.commit(500, 500);
        assert!(audio.check(1_000, 500).is_ok());
        audio.commit(1_000, 500);
        assert!(matches!(
            audio.check(1_001, 1),
            Err(RecoveryRejection::MaximumCompensation)
        ));
        assert!(video.check(1_001, 500).is_ok());
        assert!(matches!(
            audio.check(60_499, 1),
            Err(RecoveryRejection::MaximumCompensation)
        ));
        assert!(audio.check(60_500, 500).is_ok());
        audio.commit(60_500, 500);
        assert_eq!(audio.history.len(), 2);
        assert!(matches!(
            audio.check(61_000, 501),
            Err(RecoveryRejection::MaximumHole)
        ));
        assert!(!audio.clean(29_999, clock));
        assert!(audio.clean(30_000, clock));
    }

    #[test]
    fn count_limit_and_failed_checks_do_not_commit() {
        let clock = Timebase::new(nz::u32!(1), nz::u32!(1_000));
        let mut budget = CompensationBudget::new(clock, clock);
        for end in 1..=10 {
            assert!(budget.check(end, 1).is_ok());
            budget.commit(end, 1);
        }
        assert!(matches!(
            budget.check(11, 1),
            Err(RecoveryRejection::MaximumHoles)
        ));
        assert_eq!(budget.history.len(), 10);
        assert!(budget.check(60_001, 1).is_ok());
        // Preparation must not expire committed history either.
        assert_eq!(budget.history.len(), 10);
    }

    #[test]
    fn rational_clocks_and_extreme_endpoints_are_exact() {
        let endpoint = Timebase::new(nz::u32!(1001), nz::u32!(30_000));
        let duration = Timebase::new(nz::u32!(2), nz::u32!(48_000));
        let mut budget = CompensationBudget::new(endpoint, duration);
        assert!(budget.check(i64::MIN, 12_000).is_ok());
        budget.commit(i64::MIN, 12_000);
        assert!(budget.check(i64::MAX, 12_000).is_ok());
        budget.commit(i64::MAX, 12_000);
        assert_eq!(budget.history.len(), 1);
        assert!(matches!(
            budget.check(i64::MAX, 12_001),
            Err(RecoveryRejection::MaximumHole)
        ));
        assert!(!budget.clean(899, endpoint));
        assert!(budget.clean(900, endpoint));
    }
}
