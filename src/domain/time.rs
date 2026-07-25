use std::{cmp::Ordering, num::NonZeroU32, time::Duration};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

pub type TickTimestamp = i64;
pub type TickDuration = u64;
pub type TickOffset = i128;

pub fn offset_from(timestamp: TickTimestamp, origin: TickTimestamp) -> TickOffset {
    i128::from(timestamp) - i128::from(origin)
}

pub fn duration_since(timestamp: TickTimestamp, origin: TickTimestamp) -> Option<TickDuration> {
    timestamp
        .checked_sub(origin)
        .and_then(|ticks| TickDuration::try_from(ticks).ok())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timebase {
    num: NonZeroU32,
    den: NonZeroU32,
}

impl Timebase {
    pub const fn new(num: NonZeroU32, den: NonZeroU32) -> Self {
        Self { num, den }
    }

    pub const fn hz90k() -> Self {
        Self::new(nz::u32!(1), nz::u32!(90_000))
    }

    pub fn num(self) -> NonZeroU32 {
        self.num
    }

    pub fn den(self) -> NonZeroU32 {
        self.den
    }

    pub fn rescale_ticks(self, value: TickTimestamp, dst: Timebase) -> TickTimestamp {
        let num = i128::from(value)
            .saturating_mul(i128::from(self.num.get()))
            .saturating_mul(i128::from(dst.den.get()));
        let den = i128::from(self.den.get()).saturating_mul(i128::from(dst.num.get()));
        clamp_i128_to_i64(div_round_nearest(num, den))
    }

    /// Compares signed tick offsets without requantizing either value.
    pub fn compare_offsets(
        self,
        offset: TickOffset,
        other: Timebase,
        other_offset: TickOffset,
    ) -> Option<Ordering> {
        let scaled = offset
            .checked_mul(i128::from(self.num.get()))?
            .checked_mul(i128::from(other.den.get()))?;
        let other_scaled = other_offset
            .checked_mul(i128::from(other.num.get()))?
            .checked_mul(i128::from(self.den.get()))?;
        Some(scaled.cmp(&other_scaled))
    }

    pub fn duration_to_ticks(self, duration: Duration) -> TickDuration {
        let num = duration
            .as_nanos()
            .saturating_mul(u128::from(self.den.get()));
        let den = NANOS_PER_SECOND.saturating_mul(u128::from(self.num.get()));
        clamp_u128_to_u64(div_round_nearest_unsigned(num, den))
    }

    pub fn duration_to_ticks_floor(self, duration: Duration) -> TickDuration {
        let num = duration
            .as_nanos()
            .saturating_mul(u128::from(self.den.get()));
        let den = NANOS_PER_SECOND.saturating_mul(u128::from(self.num.get()));
        clamp_u128_to_u64(num / den)
    }

    pub fn duration_to_ticks_ceil(self, duration: Duration) -> TickDuration {
        let num = duration
            .as_nanos()
            .saturating_mul(u128::from(self.den.get()));
        let den = NANOS_PER_SECOND.saturating_mul(u128::from(self.num.get()));
        clamp_u128_to_u64(div_ceil_unsigned(num, den))
    }

    pub fn ticks_to_duration(self, ticks: TickDuration) -> Duration {
        if ticks == 0 {
            return Duration::ZERO;
        }

        let num = u128::from(ticks)
            .saturating_mul(u128::from(self.num.get()))
            .saturating_mul(NANOS_PER_SECOND);
        let den = u128::from(self.den.get());
        Duration::from_nanos(clamp_u128_to_u64(div_round_nearest_unsigned(num, den)))
    }

    pub fn requantize_ratio_to(self, dst: Timebase) -> Option<Timebase> {
        let num = self.num.checked_mul(dst.den)?;
        let den = self.den.checked_mul(dst.num)?;
        Some(Timebase::new(num, den))
    }
}

fn clamp_i128_to_i64(value: i128) -> i64 {
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn clamp_u128_to_u64(value: u128) -> u64 {
    value.min(u128::from(u64::MAX)) as u64
}

fn div_round_nearest(num: i128, den: i128) -> i128 {
    debug_assert!(den != 0);
    let quotient = num / den;
    let remainder = num % den;
    if remainder == 0 || remainder.abs().saturating_mul(2) < den.abs() {
        quotient
    } else if (num >= 0) == (den >= 0) {
        quotient.saturating_add(1)
    } else {
        quotient.saturating_sub(1)
    }
}

fn div_round_nearest_unsigned(num: u128, den: u128) -> u128 {
    debug_assert!(den != 0);
    let quotient = num / den;
    if (num % den).saturating_mul(2) >= den {
        quotient.saturating_add(1)
    } else {
        quotient
    }
}

fn div_ceil_unsigned(num: u128, den: u128) -> u128 {
    debug_assert!(den != 0);
    num / den + u128::from(!num.is_multiple_of(den))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_tick_rounding_is_explicit() {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(3));
        let duration = Duration::from_millis(500);

        assert_eq!(timebase.duration_to_ticks_floor(duration), 1);
        assert_eq!(timebase.duration_to_ticks(duration), 2);
        assert_eq!(timebase.duration_to_ticks_ceil(duration), 2);
    }

    #[test]
    fn adding_duration_preserves_signed_timestamp_semantics() {
        assert_eq!((-10_i64).checked_add_unsigned(5), Some(-5));
        assert_eq!(i64::MAX.checked_add_unsigned(1), None);
    }

    #[test]
    fn offset_comparison_preserves_each_timebase() {
        let video = Timebase::hz90k();
        let audio = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let video_offset = offset_from(9 * 90_000, 90_000);
        let audio_offset = offset_from(10 * 48_000, 2 * 48_000);

        assert_eq!(
            video.compare_offsets(video_offset, audio, audio_offset),
            Some(Ordering::Equal)
        );
    }
}
