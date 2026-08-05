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
    domain::{Appender, MediaInstant, TickTimestamp, Timebase, TrackId},
    media::NormalizedSample,
    segment::SegmentationPlan,
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

/// What one track's timestamps mean on the shared presentation timeline.
struct TrackClock {
    track_id: TrackId,
    timebase: Timebase,
    presentation_origin_pts: TickTimestamp,
}

/// A [`Muxer`] assembled from one independent packager per track.
pub struct TrackRouter {
    packagers: Vec<Box<dyn TrackPackager>>,
    /// Timeline mappings for the tracks this router routes, so a sample can be
    /// republished to its siblings as presentation progress.
    clocks: Vec<TrackClock>,
    expected_publication_interval: Duration,
    finished: bool,
}

impl TrackRouter {
    /// Routes to `packagers`, reading cadence and timeline mappings from the
    /// locked segmentation plan.
    pub fn new(packagers: Vec<Box<dyn TrackPackager>>, segmentation: &SegmentationPlan) -> Self {
        Self {
            packagers,
            clocks: segmentation
                .iter()
                .map(|plan| TrackClock {
                    track_id: plan.track_id,
                    timebase: plan.timebase,
                    presentation_origin_pts: plan.presentation_origin_pts,
                })
                .collect(),
            expected_publication_interval: segmentation.shortest_part_duration(),
            finished: false,
        }
    }

    /// Places a routed sample on the shared presentation timeline.
    ///
    /// `None` for a track the plan does not describe, which cannot happen for a
    /// sample this router accepted but is not worth a second error path: the
    /// only consequence is that siblings do not advance on it.
    fn instant(&self, track_id: TrackId, pts: TickTimestamp) -> Option<MediaInstant> {
        self.clocks
            .iter()
            .find(|clock| clock.track_id == track_id)
            .map(|clock| MediaInstant::new(clock.timebase, pts, clock.presentation_origin_pts))
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
        let now = self.instant(track_id, sample.pts());
        // Linear: the track set is bounded by admission policy and small, so a
        // scan beats a map both in cache behaviour and in setup cost.
        let packager = self
            .packagers
            .iter_mut()
            .find(|packager| packager.track_id() == track_id)
            .ok_or_else(|| MuxError::Mux(format!("sample references unknown {track_id}").into()))?;
        packager.push(sample, out)?;

        // Every sibling is told, rather than only the ones a designated timing
        // authority would drive. Packagers that do not need a clock default to
        // ignoring this, so the cost is a call, and not needing an authority
        // keeps calibration out of the muxer layer entirely. The price is that
        // whichever track runs furthest ahead pulls clock-driven siblings with
        // it, which stays within ordinary interleave skew.
        let Some(now) = now else {
            return Ok(());
        };
        for packager in self
            .packagers
            .iter_mut()
            .filter(|packager| packager.track_id() != track_id)
        {
            packager.tick(now, out)?;
        }
        Ok(())
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
