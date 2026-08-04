use std::{cmp::Ordering, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::time::Instant;

use crate::{
    admission::IngestTimingPolicy,
    domain::{MediaInstant, TrackId},
    observe::MediaMeters,
};

use super::{NormalizedSample, TimelineCalibration};

struct PacingWait<'a> {
    meters: &'a dyn MediaMeters,
    started: Instant,
    lead: Duration,
    completed: bool,
}

impl<'a> PacingWait<'a> {
    fn new(meters: &'a dyn MediaMeters, lead: Duration) -> Self {
        meters.pacing_observation(lead, Duration::ZERO, true);
        Self {
            meters,
            started: Instant::now(),
            lead,
            completed: false,
        }
    }

    fn complete(&mut self, media_lead: Duration) {
        let elapsed = Instant::now().saturating_duration_since(self.started);
        self.meters.pacing_observation(media_lead, elapsed, false);
        self.completed = true;
    }
}

impl Drop for PacingWait<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let elapsed = Instant::now().saturating_duration_since(self.started);
            self.meters
                .pacing_observation(self.lead.saturating_sub(elapsed), elapsed, true);
        }
    }
}

/// A sample's presentation instant, remembering which track it came from.
///
/// [`MediaInstant`] does the arithmetic and reports overflow as [`None`]; the
/// track identity lives here so a failure can name the publisher's track rather
/// than an anonymous timestamp.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MediaPoint {
    track_id: TrackId,
    instant: MediaInstant,
}

impl MediaPoint {
    fn compare(self, other: Self) -> Result<Ordering, PacingError> {
        self.instant
            .compare(other.instant)
            .ok_or(PacingError::TimestampOverflow(self.track_id))
    }

    pub(crate) fn elapsed_since(self, earlier: Self) -> Result<Duration, PacingError> {
        self.instant
            .elapsed_since(earlier.instant)
            .ok_or(PacingError::TimestampOverflow(self.track_id))
    }
}

fn point(
    sample: &NormalizedSample,
    timeline: &TimelineCalibration,
) -> Result<MediaPoint, PacingError> {
    let track_id = sample.track_id();
    let track = timeline
        .get(track_id)
        .ok_or(PacingError::UnknownTrack(track_id))?;
    Ok(MediaPoint {
        track_id,
        instant: MediaInstant::new(track.timebase, sample.pts(), track.origin_pts),
    })
}

/// A monotonic media-time watermark across track-native timebases.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MediaWatermark(Option<MediaPoint>);

impl MediaWatermark {
    pub(crate) fn observe(
        &mut self,
        sample: &NormalizedSample,
        timeline: &TimelineCalibration,
    ) -> Result<bool, PacingError> {
        let candidate = point(sample, timeline)?;
        match self.0 {
            Some(current) if candidate.compare(current)? != Ordering::Greater => Ok(false),
            _ => {
                self.0 = Some(candidate);
                Ok(true)
            }
        }
    }

    pub(crate) fn get(self) -> Option<MediaPoint> {
        self.0
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PacingError {
    #[error("cannot pace an unknown track {0}")]
    UnknownTrack(TrackId),
    #[error("timestamp arithmetic overflowed while pacing {0}")]
    TimestampOverflow(TrackId),
    #[error("normalized media jumped forward by {jump:?}, above the permitted {maximum:?}")]
    TimestampJump { maximum: Duration, jump: Duration },
    #[error("publisher led real time by {lead:?}, above the permitted {maximum:?}")]
    RealtimeLeadExceeded { maximum: Duration, lead: Duration },
}

/// Maps normalized media time onto a monotonic wall clock without rewriting it.
pub struct MediaPacer {
    policy: IngestTimingPolicy,
    timeline: TimelineCalibration,
    anchor_wall: Instant,
    anchor_media: Option<MediaPoint>,
    watermark: MediaWatermark,
    meters: Arc<dyn MediaMeters>,
}

impl MediaPacer {
    /// Starts after pre-roll, treating all buffered planning media as the
    /// bounded startup burst.
    pub fn after_preroll(
        policy: IngestTimingPolicy,
        timeline: &TimelineCalibration,
        buffered: &[NormalizedSample],
        meters: Arc<dyn MediaMeters>,
    ) -> Result<Self, PacingError> {
        let mut watermark = MediaWatermark::default();
        for sample in buffered {
            watermark.observe(sample, timeline)?;
        }
        Ok(Self {
            policy,
            timeline: timeline.clone(),
            anchor_wall: Instant::now(),
            anchor_media: watermark.get(),
            watermark,
            meters,
        })
    }

    /// Delays an ahead-of-time sample, or rejects it under realtime-only
    /// policy. A sleeping call stops the live loop from draining the next
    /// bounded input batch, which is the backpressure mechanism.
    pub async fn pace(&mut self, sample: &NormalizedSample) -> Result<(), PacingError> {
        let previous = self.watermark.get();
        // Work against a copy and commit only after any sleep finishes. The
        // supervision loop may cancel this future to service a stop or health
        // tick; committing first would let the retry mistake an unpaced sample
        // for an old B-frame and publish it immediately.
        let mut next_watermark = self.watermark;
        if !next_watermark.observe(sample, &self.timeline)? {
            return Ok(());
        }
        let current = next_watermark
            .get()
            .expect("observing an advancing sample establishes a watermark");
        let Some(anchor) = self.anchor_media else {
            self.anchor_media = Some(current);
            self.anchor_wall = Instant::now();
            self.watermark = next_watermark;
            return Ok(());
        };

        if let (
            IngestTimingPolicy::PaceToRealtime {
                maximum_timestamp_jump,
                ..
            },
            Some(previous),
        ) = (self.policy, previous)
        {
            let jump = current.elapsed_since(previous)?;
            if jump > maximum_timestamp_jump {
                return Err(PacingError::TimestampJump {
                    maximum: maximum_timestamp_jump,
                    jump,
                });
            }
        }

        let media_elapsed = current.elapsed_since(anchor)?;
        let wall_elapsed = Instant::now().saturating_duration_since(self.anchor_wall);
        let lead = media_elapsed.saturating_sub(wall_elapsed);

        match self.policy {
            IngestTimingPolicy::RequireRealtime { maximum_lead } => {
                self.meters.pacing_observation(lead, Duration::ZERO, false);
                if lead > maximum_lead {
                    return Err(PacingError::RealtimeLeadExceeded {
                        maximum: maximum_lead,
                        lead,
                    });
                }
                self.watermark = next_watermark;
            }
            IngestTimingPolicy::PaceToRealtime { maximum_lead, .. } => {
                let delay = lead.saturating_sub(maximum_lead);
                if delay.is_zero() {
                    self.meters.pacing_observation(lead, Duration::ZERO, false);
                } else {
                    let mut wait = PacingWait::new(self.meters.as_ref(), lead);
                    tokio::time::sleep(delay).await;
                    wait.complete(maximum_lead);
                }
                self.watermark = next_watermark;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        media::fixtures::{video_sample_at as sample, video_timeline as timeline},
        observe::{ProcessMeters, SessionMeters},
    };

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pacing_delays_media_beyond_the_initial_lead() {
        let timeline = timeline();
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut pacer = MediaPacer::after_preroll(
            IngestTimingPolicy::PaceToRealtime {
                maximum_lead: Duration::from_secs(1),
                maximum_timestamp_jump: Duration::from_secs(10),
            },
            &timeline,
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the anchor");

        pacer
            .pace(&sample(3))
            .await
            .expect("valid ahead-of-time media is delayed");

        let snapshot = meters.snapshot();
        assert_eq!(snapshot.pacing_delay, Duration::from_secs(2));
        assert_eq!(snapshot.media_lead, Duration::from_secs(1));
        assert!(!snapshot.publisher_backpressured);
    }

    #[tokio::test(start_paused = true)]
    async fn realtime_only_policy_rejects_excessive_lead() {
        let timeline = timeline();
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut pacer = MediaPacer::after_preroll(
            IngestTimingPolicy::RequireRealtime {
                maximum_lead: Duration::from_millis(500),
            },
            &timeline,
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the anchor");

        assert_eq!(
            pacer.pace(&sample(2)).await,
            Err(PacingError::RealtimeLeadExceeded {
                maximum: Duration::from_millis(500),
                lead: Duration::from_secs(2),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_forward_timestamp_jump_is_rejected_instead_of_slept() {
        let timeline = timeline();
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut pacer = MediaPacer::after_preroll(
            IngestTimingPolicy::PaceToRealtime {
                maximum_lead: Duration::ZERO,
                maximum_timestamp_jump: Duration::from_secs(1),
            },
            &timeline,
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the anchor");

        assert_eq!(
            pacer.pace(&sample(3)).await,
            Err(PacingError::TimestampJump {
                maximum: Duration::from_secs(1),
                jump: Duration::from_secs(3),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_retries_without_losing_or_double_pacing_the_sample() {
        let timeline = timeline();
        let meters = SessionMeters::new(ProcessMeters::default());
        let mut pacer = MediaPacer::after_preroll(
            IngestTimingPolicy::PaceToRealtime {
                maximum_lead: Duration::from_secs(1),
                maximum_timestamp_jump: Duration::from_secs(10),
            },
            &timeline,
            &[sample(0)],
            meters.media_view(),
        )
        .expect("pre-roll establishes the anchor");
        let ahead = sample(3);

        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            result = pacer.pace(&ahead) => {
                panic!("the two-second pacing wait completed early: {result:?}");
            }
        }

        let interrupted = meters.snapshot();
        assert_eq!(interrupted.pacing_delay, Duration::from_secs(1));
        assert!(interrupted.publisher_backpressured);

        pacer
            .pace(&ahead)
            .await
            .expect("the retry waits only for the remaining lead");

        let completed = meters.snapshot();
        assert_eq!(completed.pacing_delay, Duration::from_secs(2));
        assert!(!completed.publisher_backpressured);
    }
}
