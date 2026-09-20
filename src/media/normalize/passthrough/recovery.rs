use super::compensation::CompensationBudget;
#[cfg(test)]
pub use super::compensation::CompensationPolicy as AudioRecoveryPolicy;
#[cfg(test)]
use super::processing;
use crate::domain::{
    CompensationStatus, DiscoveredTrack, NormalizationNotice, RecoveryMethod, RecoveryRejection,
    RecoveryTransition, Timebase,
};
#[cfg(test)]
use crate::media::NormalizeError;

pub(super) struct Repair {
    pub ticks: u64,
    pub end: i64,
    pub missing: u64,
    pub method: RecoveryMethod,
}
pub(super) struct Recovery {
    budget: CompensationBudget,
    status: CompensationStatus,
    clean_ticks: u64,
    pub notices: Vec<NormalizationNotice>,
}
impl Recovery {
    pub fn new(
        track: &DiscoveredTrack,
        timebase: Timebase,
        mode: crate::domain::InputMode,
    ) -> Self {
        Self {
            budget: {
                let mut budget = CompensationBudget::new(timebase, timebase);
                budget.policy.enabled = mode == crate::domain::InputMode::Permissive;
                budget
            },
            clean_ticks: 0,
            notices: Vec::new(),
            status: CompensationStatus {
                media_kind: crate::domain::MediaKind::Audio,
                cadence: None,
                track: track.id,
                codec: track.codec,
                timebase,
                method: RecoveryMethod::Gap,
                missing_ticks: 0,
                replacement_ticks: 0,
                episode_holes: 0,
                episode_ticks: 0,
                total_holes: 0,
                total_ticks: 0,
                degraded: false,
            },
        }
    }
    #[cfg(test)]
    pub fn configure(&mut self, policy: AudioRecoveryPolicy) -> Result<(), NormalizeError> {
        policy.validate().map_err(processing)?;
        // Reject limits that cannot be represented by this sample clock.
        for value in [
            policy.maximum_hole,
            policy.maximum_compensation,
            policy.window,
            policy.clean_interval,
        ] {
            let ticks = value
                .as_nanos()
                .checked_mul(u128::from(self.status.timebase.den().get()))
                .and_then(|v| v.checked_div(1_000_000_000));
            if ticks.is_none_or(|v| v == 0 || v > u128::from(u64::MAX)) {
                return Err(processing(
                    "audio recovery limit cannot be represented in the sample clock",
                ));
            }
        }
        self.budget.policy = policy;
        Ok(())
    }
    pub fn prepare(&self, expected: i64, actual: i64) -> Result<Repair, RecoveryRejection> {
        use RecoveryRejection as R;
        if !self.budget.policy.enabled {
            return Err(R::Disabled);
        }
        let missing = actual.abs_diff(expected);
        if self
            .budget
            .exceeds(missing, self.budget.policy.maximum_hole)
        {
            return Err(R::MaximumHole);
        }
        if actual <= expected {
            return Err(R::UnrepresentableDuration);
        }
        let ticks = missing;
        let end = actual;
        self.budget.check(end, ticks)?;
        if self.status.total_ticks.checked_add(ticks).is_none()
            || self.status.total_holes.checked_add(1).is_none()
        {
            return Err(R::UnrepresentableDuration);
        }
        Ok(Repair {
            ticks,
            end,
            missing,
            method: RecoveryMethod::Gap,
        })
    }
    pub fn commit(&mut self, repair: &Repair) {
        self.budget.commit(repair.end, repair.ticks);
        let transition = if self.status.degraded {
            RecoveryTransition::Compensated
        } else {
            self.status.episode_holes = 0;
            self.status.episode_ticks = 0;
            RecoveryTransition::Degraded
        };
        self.status.degraded = true;
        self.status.method = repair.method;
        self.status.missing_ticks = repair.missing;
        self.status.replacement_ticks = 0;
        self.status.episode_holes += 1;
        self.status.episode_ticks += repair.ticks;
        self.status.total_holes += 1;
        self.status.total_ticks += repair.ticks;
        self.clean_ticks = 0;
        self.notices.push(NormalizationNotice {
            transition,
            status: self.status.clone(),
        });
    }
    pub fn real_audio(&mut self, duration: u64) {
        if self.status.degraded {
            self.clean_ticks = self.clean_ticks.saturating_add(duration);
            if self.budget.clean(self.clean_ticks, self.status.timebase) {
                self.status.degraded = false;
                self.notices.push(NormalizationNotice {
                    transition: RecoveryTransition::Recovered,
                    status: self.status.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaKind, fixtures::TrackBuilder};

    #[test]
    fn window_boundary_expires_repairs_and_all_limits_are_inclusive() -> Result<(), NormalizeError>
    {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let track = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .codec_extradata(&[0x11, 0x90][..])
            .build();
        let mut recovery = Recovery::new(&track, timebase, crate::domain::InputMode::Permissive);
        recovery.configure(AudioRecoveryPolicy {
            maximum_hole: std::time::Duration::from_millis(64),
            maximum_compensation: std::time::Duration::from_millis(64),
            maximum_holes: 1,
            window: std::time::Duration::from_secs(1),
            ..Default::default()
        })?;
        let first = recovery.prepare(0, 3072).expect("exact limits");
        recovery.commit(&first);
        assert!(matches!(
            recovery.prepare(48_000 - 1, 51_072 - 1),
            Err(RecoveryRejection::MaximumHoles)
        ));
        let next = recovery
            .prepare(48_000, 51_072)
            .expect("left boundary expires");
        recovery.commit(&next);

        assert_eq!(recovery.status.total_ticks, 6144);
        assert!(matches!(
            recovery.prepare(99_072, 103_168),
            Err(RecoveryRejection::MaximumHole)
        ));
        Ok(())
    }

    #[test]
    fn extreme_clock_and_limits_do_not_overflow() -> Result<(), NormalizeError> {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let track = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(timebase)
            .codec_extradata(&[0x11, 0x90][..])
            .build();
        let mut recovery = Recovery::new(&track, timebase, crate::domain::InputMode::Permissive);
        assert!(matches!(
            recovery.prepare(i64::MIN, i64::MAX),
            Err(RecoveryRejection::MaximumHole)
        ));
        assert!(
            recovery
                .configure(AudioRecoveryPolicy {
                    window: std::time::Duration::MAX,
                    ..Default::default()
                })
                .is_err()
        );
        recovery.configure(AudioRecoveryPolicy::default())?;
        assert_eq!(
            recovery
                .prepare(i64::MAX - 2000, i64::MAX)
                .expect("exact endpoint fits")
                .end,
            i64::MAX
        );
        Ok(())
    }
}
