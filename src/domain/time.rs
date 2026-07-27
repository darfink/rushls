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

    /// Requantizes a signed timestamp without hiding arithmetic overflow.
    ///
    /// Media timestamps may legitimately be negative for priming or reordered
    /// video. Returning `None` at either arithmetic boundary lets ingest reject
    /// an unrepresentable clock instead of silently pinning it to an endpoint.
    pub fn checked_rescale_ticks(
        self,
        value: TickTimestamp,
        dst: Timebase,
    ) -> Option<TickTimestamp> {
        let numerator = i128::from(value)
            .checked_mul(i128::from(self.num.get()))?
            .checked_mul(i128::from(dst.den.get()))?;
        let denominator = i128::from(self.den.get()).checked_mul(i128::from(dst.num.get()))?;
        let rounded = div_round_nearest_checked(numerator, denominator)?;
        TickTimestamp::try_from(rounded).ok()
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

/// Requantizes ordered tick deltas while carrying their fractional remainder.
///
/// Absolute timestamps and observed intervals should be projected from their
/// endpoints with [`Timebase::checked_rescale_ticks`]. This stateful form is
/// for clocks that must synthesize successive durations without an absolute
/// endpoint to anchor every step, such as a declared fractional frame cadence.
///
/// The initial half-denominator makes the accumulated boundary use nearest
/// rounding. Consequently, the sum of any uninterrupted run stays within half
/// an output tick of the exact rational duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RationalTickAccumulator {
    whole_per_input_tick: u128,
    remainder_per_input_tick: u128,
    denominator: u128,
    remainder: u128,
}

impl RationalTickAccumulator {
    /// Builds an accumulator converting deltas from `src` ticks to `dst` ticks.
    pub fn requantize_to_ticks(src: Timebase, dst: Timebase) -> Option<Self> {
        let numerator = u128::from(src.num.get()).checked_mul(u128::from(dst.den.get()))?;
        let denominator = u128::from(src.den.get()).checked_mul(u128::from(dst.num.get()))?;
        let divisor = greatest_common_divisor(numerator, denominator);
        let numerator = numerator / divisor;
        let denominator = denominator / divisor;
        Some(Self {
            whole_per_input_tick: numerator / denominator,
            remainder_per_input_tick: numerator % denominator,
            denominator,
            remainder: denominator / 2,
        })
    }

    /// Restarts nearest-boundary rounding at a new synthesized clock anchor.
    pub fn reset(&mut self) {
        self.remainder = self.denominator / 2;
    }

    /// Advances the synthesized clock, rejecting rather than clamping overflow.
    ///
    /// State changes only after the result is known to fit, so callers may
    /// report an error without leaving the clock at a partially advanced phase.
    pub fn advance(&mut self, delta_ticks: TickDuration) -> Option<TickDuration> {
        let delta = u128::from(delta_ticks);
        let whole = delta.checked_mul(self.whole_per_input_tick)?;
        let fractional = delta
            .checked_mul(self.remainder_per_input_tick)?
            .checked_add(self.remainder)?;
        let output = whole.checked_add(fractional / self.denominator)?;
        let output = TickDuration::try_from(output).ok()?;
        self.remainder = fractional % self.denominator;
        Some(output)
    }
}

fn greatest_common_divisor(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
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

fn div_round_nearest_checked(num: i128, den: i128) -> Option<i128> {
    debug_assert!(den != 0);
    let quotient = num.checked_div(den)?;
    let remainder = num.checked_rem(den)?;
    if remainder == 0 || remainder.abs().checked_mul(2)? < den.abs() {
        Some(quotient)
    } else if (num >= 0) == (den >= 0) {
        quotient.checked_add(1)
    } else {
        quotient.checked_sub(1)
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

    #[test]
    fn checked_rescaling_preserves_negative_timestamps_and_rejects_overflow() {
        let milliseconds = Timebase::new(nz::u32!(1), nz::u32!(1_000));

        assert_eq!(
            milliseconds.checked_rescale_ticks(-22, Timebase::hz90k()),
            Some(-1_980)
        );
        assert_eq!(
            Timebase::new(nz::u32!(u32::MAX), nz::u32!(1))
                .checked_rescale_ticks(i64::MAX, Timebase::new(nz::u32!(1), nz::u32!(u32::MAX))),
            None
        );
    }

    #[test]
    fn rational_tick_accumulation_preserves_fractional_cadence_without_drift() {
        let frame_period = Timebase::new(nz::u32!(1_001), nz::u32!(24_000));
        let mut clock =
            RationalTickAccumulator::requantize_to_ticks(frame_period, Timebase::hz90k())
                .expect("the cadence ratio is representable");

        let durations: Vec<_> = (0..8)
            .map(|_| clock.advance(1).expect("one frame fits"))
            .collect();

        assert_eq!(durations.iter().sum::<TickDuration>(), 30_030);
        assert_eq!(
            durations,
            [3_754, 3_754, 3_753, 3_754, 3_754, 3_754, 3_753, 3_754]
        );
        clock.reset();
        assert_eq!(clock.advance(1), Some(durations[0]));
    }

    #[test]
    fn failed_rational_tick_advance_does_not_change_its_phase() {
        let src = Timebase::new(nz::u32!(u32::MAX), nz::u32!(2));
        let dst = Timebase::new(nz::u32!(1), nz::u32!(u32::MAX));
        let mut clock =
            RationalTickAccumulator::requantize_to_ticks(src, dst).expect("ratio is valid");
        let mut fresh = clock;

        assert_eq!(clock.advance(3), None);
        assert_eq!(clock.advance(1), fresh.advance(1));
    }
}
