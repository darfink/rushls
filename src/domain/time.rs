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

    /// Converts ticks to the nearest nanosecond representable by [`Duration`].
    ///
    /// Rounding is deliberately independent for each call so this conversion
    /// remains deterministic and context-free. Repeated rational durations can
    /// therefore accumulate at most half a nanosecond of representation error
    /// per value, which is negligible even for long-running media timelines.
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
}

/// A checked projection from one tick domain into another.
///
/// Keeping the pair together makes interval conversion use the same mapping as
/// timestamp conversion. Projecting both interval endpoints ensures adjacent
/// intervals remain adjacent instead of accumulating independently rounded
/// durations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimebaseProjection {
    source: Timebase,
    destination: Timebase,
}

impl TimebaseProjection {
    pub const fn new(source: Timebase, destination: Timebase) -> Self {
        Self {
            source,
            destination,
        }
    }

    pub fn timestamp(self, value: TickTimestamp) -> Option<TickTimestamp> {
        self.source.checked_rescale_ticks(value, self.destination)
    }

    pub fn interval(
        self,
        start: TickTimestamp,
        duration: TickDuration,
    ) -> Option<(TickTimestamp, TickDuration)> {
        let end = start.checked_add_unsigned(duration)?;
        let projected_start = self.timestamp(start)?;
        let projected_end = self.timestamp(end)?;
        let projected_duration = projected_end
            .checked_sub(projected_start)
            .and_then(|duration| TickDuration::try_from(duration).ok())
            .filter(|duration| *duration > 0)?;
        Some((projected_start, projected_duration))
    }

    /// The ceiling of one source tick measured in destination ticks.
    pub fn one_source_tick_ceil(self) -> Option<TickDuration> {
        let numerator = u128::from(self.source.num.get())
            .checked_mul(u128::from(self.destination.den.get()))?;
        let denominator = u128::from(self.source.den.get())
            .checked_mul(u128::from(self.destination.num.get()))?;
        let ticks = numerator
            .checked_add(denominator.checked_sub(1)?)?
            .checked_div(denominator)?;
        TickDuration::try_from(ticks).ok()
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

fn clamp_u128_to_u64(value: u128) -> u64 {
    u64::try_from(value.min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// Builds a [`Duration`] from nanoseconds, saturating instead of overflowing.
///
/// Shared by the delivery store's policy math, where several independent
/// rules scale durations (retention windows, part floors) and none of them
/// may fail because a configured multiple produced a value larger than a
/// [`Duration`] can represent.
pub fn duration_from_nanos_saturating(nanos: u128) -> Duration {
    let nanos = nanos.min(Duration::MAX.as_nanos());
    let seconds = u64::try_from(nanos / NANOS_PER_SECOND).unwrap_or(u64::MAX);
    let subsecond_nanos = u32::try_from(nanos % NANOS_PER_SECOND).unwrap_or(u32::MAX);
    Duration::new(seconds, subsecond_nanos)
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
    fn interval_projection_uses_absolute_endpoints() {
        let projection = TimebaseProjection::new(
            Timebase::new(nz::u32!(1), nz::u32!(10_000)),
            Timebase::hz90k(),
        );

        assert_eq!(projection.interval(333, 334), Some((2_997, 3_006)));
        assert_eq!(projection.one_source_tick_ceil(), Some(9));
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
