//! Packaging one track at a time, and routing an interleaved stream to many.
//!
//! [`Muxer`] takes the whole publication because a transcoding implementation
//! genuinely needs it: one input track may become three renditions of a ladder,
//! and one rendition may be muxed from an audio and a video track together.
//! Neither relationship survives a per-track trait, so the seam stays where it
//! is.
//!
//! Pass-through packaging is the opposite case — every output is one input
//! track, container state is per-track, and there is nothing to decide across
//! them. A packager written that way should not also have to implement routing.
//! [`TrackRouter`] supplies it once.

use std::time::Duration;

use crate::{
    domain::{Appender, TrackId},
    media::NormalizedSample,
};

use super::{FinishReason, MuxError, Muxer, PackagedMedia};

/// Packages one input track's samples into container objects.
pub trait TrackPackager: Send {
    /// The input track this packager consumes. Routing is by this alone, so it
    /// must not change over the packager's life.
    fn track_id(&self) -> TrackId;

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError>;

    /// Closes out this track, on the same terms as [`Muxer::finish`].
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

/// A [`Muxer`] assembled from one independent packager per track.
pub struct TrackRouter {
    packagers: Vec<Box<dyn TrackPackager>>,
    expected_publication_interval: Duration,
    finished: bool,
}

impl TrackRouter {
    pub fn new(
        packagers: Vec<Box<dyn TrackPackager>>,
        expected_publication_interval: Duration,
    ) -> Self {
        Self {
            packagers,
            expected_publication_interval,
            finished: false,
        }
    }
}

impl Muxer for TrackRouter {
    fn expected_publication_interval(&self) -> Duration {
        self.expected_publication_interval
    }

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Err(MuxError::Mux(
                "cannot push media after the muxer finished".into(),
            ));
        }
        let track_id = sample.track_id();
        // Linear: the track set is bounded by admission policy and small, so a
        // scan beats a map both in cache behaviour and in setup cost.
        let packager = self
            .packagers
            .iter_mut()
            .find(|packager| packager.track_id() == track_id)
            .ok_or_else(|| MuxError::Mux(format!("sample references unknown {track_id}").into()))?;
        packager.push(sample, out)
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // Every packager is finished even after one fails. Stopping early would
        // strand the rest holding container state that only `finish` releases,
        // and would drop media a healthy track had already accepted.
        let mut first_error = None;
        for packager in &mut self.packagers {
            if let Err(error) = packager.finish(reason, out)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
