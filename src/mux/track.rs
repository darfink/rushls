//! Packaging one track at a time, and routing an interleaved stream to many.
//!
//! [`super::Muxer`] takes the whole publication because a transcoding implementation
//! genuinely needs it: one input track may become three renditions of a ladder,
//! and one rendition may be muxed from an audio and a video track together.
//! Neither relationship survives a per-track trait, so the seam stays where it
//! is.
//!
//! The presentation coordinator selects boundaries before routing samples.

use super::{FinishReason, MuxError, PackagedMedia};
use crate::{
    domain::{Appender, MediaInstant, TickTimestamp, TrackId},
    media::NormalizedMedia,
};

/// Packages one input track's samples into container objects.
pub trait TrackPackager: Send {
    /// Retained unpublished samples, counted with coordinator queues against
    /// a single publication limit rather than once per rendition. Bytes are
    /// bounded by the shared pipeline budget instead.
    fn buffered(&self) -> usize {
        0
    }

    /// The input track this packager consumes. Routing is by this alone, so it
    /// must not change over the packager's life.
    fn track_id(&self) -> TrackId;

    fn push(
        &mut self,
        sample: NormalizedMedia,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError>;

    /// Reserves the output memory `sample` will need, at the moment the muxer
    /// accepts it. The charge travels with the sample through every queue to
    /// [`Self::push_reserved`], so releasing held samples later, including
    /// while draining at finish, never has to allocate.
    fn reserve(
        &self,
        _sample: &NormalizedMedia,
    ) -> Result<Option<crate::domain::Reservation>, MuxError> {
        Ok(None)
    }

    /// Pushes a sample whose output was reserved by [`Self::reserve`].
    fn push_reserved(
        &mut self,
        sample: NormalizedMedia,
        _charge: Option<crate::domain::Reservation>,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        self.push(sample, out)
    }

    /// Commits a selected boundary in the source track's timestamp domain.
    fn cut(
        &mut self,
        _pts: TickTimestamp,
        _out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        Ok(())
    }

    /// Advances a packager to where the presentation has reached.
    ///
    /// Output whose cadence is owed to the timeline rather than driven by its
    /// own input needs this: a sparse subtitle track produces nothing between
    /// cues, yet HLS requires its playlist to keep pace with its siblings, and
    /// a playlist that does not advance stalls blocking reloads until they time
    /// out. Packagers whose input already carries their cadence ignore it.
    ///
    /// `now` may go backwards between calls, because siblings interleave and
    /// each carries its own clock. Implementations decide what to do about
    /// that; the router does not filter.
    fn tick(
        &mut self,
        _now: MediaInstant,
        _out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        Ok(())
    }

    /// Closes out this track, on the same terms as [`super::Muxer::finish`].
    ///
    /// Called exactly once, and called even when a sibling has already failed,
    /// so an implementation holding an external resource can rely on it to
    /// release that resource.
    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError>;
}
