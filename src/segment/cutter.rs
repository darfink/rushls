//! Shared duration decisions for admission replay and live CMAF packaging.
//!
//! Independence permits a short part, but never an oversized one. Arithmetic
//! is widened before scaling so large hostile timestamps cannot wrap a limit.

use crate::domain::{MediaKind, TickDuration, TickTimestamp, duration_since};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CutError {
    #[error("part repair exceeded the unpublished window or work budget, ceiling {maximum}")]
    RepairWindowExhausted { maximum: TickDuration },
    #[error("sample duration {duration} exceeds part ceiling {maximum}")]
    SampleTooLong {
        duration: TickDuration,
        maximum: TickDuration,
    },
    #[error("part duration {duration} exceeds part ceiling {maximum}")]
    PartTooLong {
        duration: TickDuration,
        maximum: TickDuration,
    },
    #[error("no legal part partition: duration {duration}, ceiling {maximum}")]
    NoPartition {
        duration: TickDuration,
        maximum: TickDuration,
    },
}

pub fn regular(duration: TickDuration, maximum: TickDuration, independent: bool) -> bool {
    duration > 0
        && duration <= maximum
        && (independent || u128::from(duration) * 100 >= u128::from(maximum) * 85)
}

/// Whether to close the current part before accepting the next sample.
pub fn before_sample(
    current: TickDuration,
    next: TickDuration,
    independent: bool,
    maximum: TickDuration,
) -> Result<bool, CutError> {
    if next > maximum {
        return Err(CutError::SampleTooLong {
            duration: next,
            maximum,
        });
    }
    if current > maximum {
        return Err(CutError::PartTooLong {
            duration: current,
            maximum,
        });
    }
    if current == 0 {
        return Ok(false);
    }
    let threshold = u128::from(current) * 100 >= u128::from(maximum) * 85;
    let overflow = u128::from(current) + u128::from(next) > u128::from(maximum);
    if overflow && !regular(current, maximum, independent) {
        return Err(CutError::NoPartition {
            duration: current,
            maximum,
        });
    }
    Ok(threshold || overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_parts_can_close_early_but_cannot_overflow() {
        assert_eq!(before_sample(60, 50, true, 100), Ok(true));
        assert!(matches!(
            before_sample(60, 50, false, 100),
            Err(CutError::NoPartition { .. })
        ));
        assert!(before_sample(0, 101, true, 100).is_err());
    }

    #[test]
    fn cfr_and_vfr_share_the_same_bounds() -> Result<(), CutError> {
        for durations in [vec![33; 90], [40, 20, 50, 10].repeat(30)] {
            let mut current = 0;
            for next in durations {
                if before_sample(current, next, false, 250)? {
                    assert!(regular(current, 250, false));
                    current = 0;
                }
                current += next;
                assert!(current <= 250);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    #[test]
    fn fractional_cfr_and_later_vfr_keep_the_same_part_ceiling() -> Result<(), CutError> {
        for frame in [3_600, 3_000, 3_003] {
            let mut duration = 0;
            for sample in [frame; 120]
                .into_iter()
                .chain([1_800, 4_500, 2_700].repeat(40))
            {
                if before_sample(duration, sample, false, 45_000)? {
                    assert!(regular(duration, 45_000, false));
                    duration = 0;
                }
                duration += sample;
                assert!(duration <= 45_000);
            }
        }
        Ok(())
    }
}

/// Converts presented access units to the spans the container accounts for.
/// Audio includes the encoded origin before priming; video follows decode durations.
pub struct PartClock {
    audio: bool,
    end: TickTimestamp,
}

impl PartClock {
    pub fn new(kind: MediaKind, origin: TickTimestamp) -> Self {
        Self {
            audio: kind == MediaKind::Audio,
            end: origin,
        }
    }

    pub fn advance(&mut self, pts: TickTimestamp, duration: TickDuration) -> Option<TickDuration> {
        if !self.audio || duration == 0 {
            return Some(duration);
        }
        let end = pts.checked_add_unsigned(duration)?.max(self.end);
        let span = duration_since(end, self.end)?;
        self.end = end;
        Some(span)
    }
}

/// Timing retained until a part boundary can no longer be repaired.
#[derive(Clone, Copy, Debug)]
struct Unit {
    duration: TickDuration,
    independent: bool,
}

/// One-access-unit lookahead over unpublished parts, shared by replay and CMAF.
/// Payloads stay with the writer; returned counts always refer to its queue head.
pub struct PartPartitioner {
    units: Vec<Unit>,
    maximum: TickDuration,
    tentative_at: Option<usize>,
    committed: bool,
}

impl PartPartitioner {
    pub fn new(maximum: TickDuration) -> Self {
        Self {
            units: Vec::new(),
            maximum,
            tentative_at: None,
            committed: false,
        }
    }

    pub fn push(
        &mut self,
        duration: TickDuration,
        independent: bool,
    ) -> Result<Vec<usize>, CutError> {
        if duration > self.maximum {
            return Err(CutError::SampleTooLong {
                duration,
                maximum: self.maximum,
            });
        }
        self.units.push(Unit {
            duration,
            independent,
        });
        let mut cuts = Vec::new();
        loop {
            let partition = self.partition()?;
            let total: u128 = self
                .units
                .iter()
                .map(|unit| u128::from(unit.duration))
                .sum();
            if partition.len() < 2 {
                self.tentative_at = None;
                break;
            }
            // The triggering sample belongs to the successor. One more sample
            // can expose a bad greedy cut while both parts are still editable.
            // A one-unit part that cannot hold its successor has no alternate
            // cut. Independent units also need no repair lookahead: every
            // following part can legally close below the 85% floor.
            let irrevocable = (partition[0] == 1
                && u128::from(self.units[0].duration) + u128::from(self.units[1].duration)
                    > u128::from(self.maximum))
                || self.units.iter().all(|unit| unit.independent);
            let ready = irrevocable
                || self.tentative_at.is_some_and(|at| self.units.len() > at)
                || total > u128::from(self.maximum) * 2;
            if !ready {
                self.tentative_at = Some(self.units.len());
                break;
            }
            let count = partition[0];
            self.units.drain(..count);
            self.committed = true;
            self.tentative_at = None;
            cuts.push(count);
        }
        Ok(cuts)
    }

    /// Only a real segment end grants the short-final-part exemption.
    pub fn finish(&mut self) -> Result<Vec<usize>, CutError> {
        let partition = self.partition()?;
        let mut previous = 0;
        let counts = partition
            .into_iter()
            .map(|end| {
                let count = end - previous;
                previous = end;
                count
            })
            .collect();
        self.units.clear();
        self.tentative_at = None;
        self.committed = false;
        Ok(counts)
    }

    fn partition(&self) -> Result<Vec<usize>, CutError> {
        if self.units.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(greedy) = self.greedy() {
            return Ok(greedy);
        }
        self.repair()
    }

    fn greedy(&self) -> Option<Vec<usize>> {
        let mut duration = 0_u64;
        let mut independent = self.units[0].independent;
        let mut ends = Vec::new();
        for (index, unit) in self.units.iter().enumerate() {
            if before_sample(duration, unit.duration, independent, self.maximum).ok()? {
                ends.push(index);
                duration = 0;
                independent = unit.independent;
            }
            duration = duration.checked_add(unit.duration)?;
        }
        ends.push(self.units.len());
        Some(ends)
    }

    /// Minimum-part partition, with longer earlier parts breaking ties. The
    /// open suffix is extendable, not yet published under the final exemption.
    fn repair(&self) -> Result<Vec<usize>, CutError> {
        let n = self.units.len();
        let mut cost = vec![usize::MAX; n + 1];
        let mut next = vec![0; n];
        cost[n] = 0;
        let mut work = 0_usize;
        for start in (0..n).rev() {
            let mut sum = 0_u64;
            for end in start..n {
                work += 1;
                if work > 1_000_000 {
                    return Err(CutError::RepairWindowExhausted {
                        maximum: self.maximum,
                    });
                }
                let Some(duration) = sum.checked_add(self.units[end].duration) else {
                    break;
                };
                sum = duration;
                if sum > self.maximum {
                    break;
                }
                let legal = sum > 0
                    && (end + 1 == n || regular(sum, self.maximum, self.units[start].independent));
                if legal && cost[end + 1] != usize::MAX && cost[end + 1] < cost[start] {
                    cost[start] = cost[end + 1] + 1;
                    next[start] = end + 1;
                }
            }
        }
        if cost[0] == usize::MAX {
            return Err(if self.committed {
                CutError::RepairWindowExhausted {
                    maximum: self.maximum,
                }
            } else {
                CutError::NoPartition {
                    duration: self
                        .units
                        .iter()
                        .fold(0_u64, |sum, unit| sum.saturating_add(unit.duration)),
                    maximum: self.maximum,
                }
            });
        }
        let mut ends = Vec::new();
        let mut start = 0;
        while start < n {
            start = next[start];
            ends.push(start);
        }
        Ok(ends)
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;

    /// Enumerates every legal partition independently of the dynamic program.
    fn oracle(units: &[Unit], start: usize, maximum: u64) -> Option<Vec<usize>> {
        if start == units.len() {
            return Some(Vec::new());
        }
        let mut best: Option<Vec<usize>> = None;
        let mut sum = 0;
        for end in start..units.len() {
            sum += units[end].duration;
            if sum > maximum {
                break;
            }
            if sum == 0
                || (end + 1 != units.len() && !regular(sum, maximum, units[start].independent))
            {
                continue;
            }
            if let Some(mut tail) = oracle(units, end + 1, maximum) {
                tail.insert(0, end + 1);
                if best.as_ref().is_none_or(|old| {
                    tail.len() < old.len() || (tail.len() == old.len() && tail > *old)
                }) {
                    best = Some(tail);
                }
            }
        }
        best
    }

    #[test]
    fn bounded_repair_matches_an_exhaustive_oracle() -> Result<(), CutError> {
        let durations = [10, 20, 45, 55, 60, 85, 100];
        for encoded in 0..durations.len().pow(4) {
            for flags in 0..16 {
                let mut code = encoded;
                let units: Vec<_> = (0..4)
                    .map(|index| {
                        let duration = durations[code % durations.len()];
                        code /= durations.len();
                        Unit {
                            duration,
                            independent: flags & (1 << index) != 0,
                        }
                    })
                    .collect();
                if units.iter().map(|unit| unit.duration).sum::<u64>() > 200 {
                    continue;
                }
                let expected = oracle(&units, 0, 100);
                let mut planner = PartPartitioner::new(100);
                planner.units = units;
                match expected {
                    Some(expected) => assert_eq!(planner.repair()?, expected),
                    None => assert!(planner.repair().is_err()),
                }
            }
        }
        Ok(())
    }

    #[test]
    fn lookahead_repairs_only_unpublished_parts() -> Result<(), CutError> {
        let mut planner = PartPartitioner::new(100);
        for (duration, independent) in [(20, true), (55, false), (45, false)] {
            assert!(planner.push(duration, independent)?.is_empty());
        }
        assert_eq!(planner.push(60, false)?, [1]);
        assert_eq!(planner.finish()?, [2, 1]);
        assert!(planner.push(101, true).is_err());
        Ok(())
    }

    #[test]
    fn cfr_vfr_and_missing_frames_stay_inside_the_repair_window() -> Result<(), CutError> {
        let mut planner = PartPartitioner::new(1000);
        for index in 0..900 {
            let duration = match index % 31 {
                0 => 66,
                1 => 20,
                2 => 47,
                _ => 33,
            };
            planner.push(duration, index % 60 == 0)?;
            assert!(planner.units.iter().map(|unit| unit.duration).sum::<u64>() <= 2000);
        }
        planner.finish()?;
        Ok(())
    }

    #[test]
    fn single_unit_parts_do_not_wait_when_the_cut_is_forced() -> Result<(), CutError> {
        let mut planner = PartPartitioner::new(100);
        assert!(planner.push(90, true)?.is_empty());
        assert_eq!(planner.push(90, false)?, [1]);
        assert_eq!(planner.push(90, false)?, [1]);
        assert_eq!(planner.finish()?, [1]);
        Ok(())
    }

    #[test]
    fn an_exhausted_published_window_fails_without_rewriting() -> Result<(), CutError> {
        let mut planner = PartPartitioner::new(100);
        assert!(planner.push(85, true)?.is_empty());
        assert!(planner.push(15, false)?.is_empty());
        assert_eq!(planner.push(70, false)?, [1]);
        assert!(planner.push(40, false)?.is_empty());
        assert!(matches!(
            planner.push(65, false),
            Err(CutError::RepairWindowExhausted { .. })
        ));
        Ok(())
    }
}
