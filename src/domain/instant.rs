//! Comparing presentation instants across tracks that do not share a clock.
//!
//! Every track carries its own [`Timebase`] — 90 kHz video next to 48 kHz
//! audio is the ordinary case — and normalized timestamps are never requantized
//! into a canonical clock, because doing so would accumulate rounding error into
//! the very boundaries segmentation depends on being exact.
//!
//! So "is this video access unit at the same instant as that audio one?" is a
//! rational comparison, not a subtraction. [`MediaInstant`] is that comparison
//! given a name, so pacing, density accounting, and boundary selection stop
//! each deriving it from [`Timebase::compare_offsets`] on their own.
//!
//! # Origins
//!
//! An instant is stored as an offset from its track's presentation origin, not
//! as a raw PTS. That is what makes two tracks comparable when their inputs
//! started at different timestamps. It also means this type is unaffected by
//! whether normalization eventually rebases each track to zero and expresses
//! the shift as an edit list: with a zero origin the offset simply equals the
//! PTS, and every caller here keeps working unchanged.

use std::{cmp::Ordering, time::Duration};

use super::{TickOffset, TickTimestamp, Timebase, offset_from};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// One presentation instant, expressed in a single track's tick domain.
///
/// Comparison and elapsed-time return [`Option`] rather than a domain error:
/// the only failure is arithmetic overflow, and each layer already has its own
/// vocabulary for reporting that against its own track identity. Returning
/// someone else's error type here is what previously made media-density
/// accounting report a *pacing* failure.
#[derive(Clone, Copy, Debug)]
pub struct MediaInstant {
    timebase: Timebase,
    offset: TickOffset,
}

impl MediaInstant {
    /// Places `pts` relative to the origin its track was calibrated onto.
    pub fn new(timebase: Timebase, pts: TickTimestamp, origin: TickTimestamp) -> Self {
        Self {
            timebase,
            offset: offset_from(pts, origin),
        }
    }

    pub fn timebase(self) -> Timebase {
        self.timebase
    }

    pub fn offset(self) -> TickOffset {
        self.offset
    }

    /// Orders two instants exactly, without requantizing either one.
    ///
    /// `None` only on overflow, which needs values far outside any real media
    /// timeline.
    pub fn compare(self, other: Self) -> Option<Ordering> {
        self.timebase
            .compare_offsets(self.offset, other.timebase, other.offset)
    }

    /// Wall-clock time from `earlier` to `self`, or `None` if that is negative
    /// or overflows.
    ///
    /// Saturates at [`u64::MAX`] nanoseconds rather than failing: a duration
    /// that large is already meaningless, and every caller treats it as "far
    /// beyond any limit" regardless.
    pub fn elapsed_since(self, earlier: Self) -> Option<Duration> {
        let elapsed = self.nanos()?.checked_sub(earlier.nanos()?)?;
        u64::try_from(elapsed)
            .ok()
            .map(Duration::from_nanos)
            .or_else(|| (elapsed > 0).then_some(Duration::from_nanos(u64::MAX)))
    }

    fn nanos(self) -> Option<i128> {
        self.offset
            .checked_mul(i128::from(self.timebase.num().get()))?
            .checked_mul(NANOS_PER_SECOND)
            .map(|value| value / i128::from(self.timebase.den().get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(pts: TickTimestamp, origin: TickTimestamp) -> MediaInstant {
        MediaInstant::new(Timebase::hz90k(), pts, origin)
    }

    fn audio(pts: TickTimestamp, origin: TickTimestamp) -> MediaInstant {
        MediaInstant::new(Timebase::new(nz::u32!(1), nz::u32!(48_000)), pts, origin)
    }

    #[test]
    fn tracks_on_different_clocks_compare_by_presentation_time() {
        // Video 8s past a 1s origin; audio 8s past a 2s origin. Same instant.
        assert_eq!(
            video(9 * 90_000, 90_000).compare(audio(10 * 48_000, 2 * 48_000)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            video(10 * 90_000, 90_000).compare(audio(10 * 48_000, 2 * 48_000)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn elapsed_time_crosses_tick_domains() {
        assert_eq!(
            audio(10 * 48_000, 0).elapsed_since(video(90_000, 0)),
            Some(Duration::from_secs(9))
        );
    }

    #[test]
    fn going_backwards_has_no_elapsed_time() {
        assert_eq!(video(0, 0).elapsed_since(video(90_000, 0)), None);
    }

    #[test]
    fn a_zero_origin_leaves_the_offset_equal_to_the_timestamp() {
        // Guards the rebase-to-zero option: nothing here depends on a
        // non-trivial origin.
        assert_eq!(video(9 * 90_000, 0).offset(), 9 * 90_000);
        assert_eq!(
            video(9 * 90_000, 0).elapsed_since(video(0, 0)),
            Some(Duration::from_secs(9))
        );
    }
}
