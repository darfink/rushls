use std::time::Duration;

use crate::{
    domain::{MediaInstant, TrackId},
    source::{DensityUnit, InputLimits, LimitError},
};

use super::{NormalizedSample, TimelineCalibration};

/// Fixed media-time accounting for one publisher.
///
/// Unlike an arrival-rate limiter, this window advances only when normalized
/// media time advances. Replaying ten valid seconds quickly is therefore fine;
/// sending unbounded packets or samples at one timestamp is not.
///
/// Tracks the media clock itself rather than borrowing the pacer's watermark:
/// the two ask different questions of the same timestamps, and sharing the
/// answer made a density failure report itself as a pacing failure.
pub struct MediaDensityWindow {
    limits: InputLimits,
    timeline: TimelineCalibration,
    window_start: Option<MediaInstant>,
    latest: Option<(TrackId, MediaInstant)>,
    packets: u64,
    samples: u64,
}

impl MediaDensityWindow {
    pub fn new(limits: InputLimits, timeline: &TimelineCalibration) -> Self {
        Self {
            limits,
            timeline: timeline.clone(),
            window_start: None,
            latest: None,
            packets: 0,
            samples: 0,
        }
    }

    pub fn admit(
        &mut self,
        packets: u64,
        samples: &[NormalizedSample],
    ) -> Result<(), MediaDensityError> {
        for sample in samples {
            self.observe(sample)?;
        }

        if let Some((track_id, current)) = self.latest {
            let elapsed = |start| {
                current
                    .elapsed_since(start)
                    .ok_or(MediaDensityError::TimestampOverflow(track_id))
            };
            match self.window_start {
                None => self.window_start = Some(current),
                // Fixed, non-overlapping windows: once media time has advanced
                // a full window, everything before it stops counting.
                Some(start) if elapsed(start)? >= self.limits.media_density_window => {
                    self.window_start = Some(current);
                    self.packets = 0;
                    self.samples = 0;
                }
                Some(_) => {}
            }
        }

        self.packets = self.packets.saturating_add(packets);
        self.samples = self
            .samples
            .saturating_add(u64::try_from(samples.len()).unwrap_or(u64::MAX));

        check(
            self.packets,
            self.limits.maximum_packets_per_media_second,
            self.limits.media_density_window,
            DensityUnit::Packets,
        )?;
        check(
            self.samples,
            self.limits.maximum_samples_per_media_second,
            self.limits.media_density_window,
            DensityUnit::Samples,
        )?;
        Ok(())
    }

    /// Advances the media clock, ignoring samples that do not move it forward.
    ///
    /// Reordered access units and interleaved tracks both go backwards
    /// routinely; only the furthest point reached defines how much media time
    /// the publisher has actually spent.
    fn observe(&mut self, sample: &NormalizedSample) -> Result<(), MediaDensityError> {
        let track_id = sample.track_id();
        let track = self
            .timeline
            .get(track_id)
            .ok_or(MediaDensityError::UnknownTrack(track_id))?;
        let candidate = MediaInstant::new(track.timebase, sample.pts(), track.origin_pts);
        let advances = match self.latest {
            Some((_, current)) => candidate
                .compare(current)
                .ok_or(MediaDensityError::TimestampOverflow(track_id))?
                .is_gt(),
            None => true,
        };
        if advances {
            self.latest = Some((track_id, candidate));
        }
        Ok(())
    }
}

fn check(
    observed: u64,
    per_second_limit: u64,
    window: Duration,
    unit: DensityUnit,
) -> Result<(), LimitError> {
    let allowed = scale(per_second_limit, window);
    if observed > allowed {
        return Err(LimitError::MediaDensityExceeded {
            unit,
            limit: per_second_limit,
            observed: per_second(observed, window),
        });
    }
    Ok(())
}

fn scale(per_second: u64, window: Duration) -> u64 {
    let scaled = u128::from(per_second) * window.as_micros() / 1_000_000;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

fn per_second(observed: u64, window: Duration) -> u64 {
    let micros = window.as_micros().max(1);
    let rate = u128::from(observed) * 1_000_000 / micros;
    u64::try_from(rate).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum MediaDensityError {
    #[error("media density accounting received a sample for unknown {0}")]
    UnknownTrack(TrackId),
    #[error("timestamp arithmetic overflowed while accounting for {0}")]
    TimestampOverflow(TrackId),
    #[error(transparent)]
    Limit(#[from] LimitError),
}

#[cfg(test)]
mod tests {
    use crate::media::fixtures::{video_sample_at as sample, video_timeline as timeline};

    use super::*;

    fn limits() -> InputLimits {
        InputLimits {
            maximum_packets_per_media_second: 2,
            maximum_samples_per_media_second: 2,
            media_density_window: Duration::from_secs(1),
            ..InputLimits::permissive()
        }
    }

    #[test]
    fn identical_timestamps_cannot_hide_unbounded_packet_density() {
        let mut density = MediaDensityWindow::new(limits(), &timeline());
        density
            .admit(2, &[sample(0)])
            .expect("the first media window is within its cap");

        assert_eq!(
            density.admit(1, &[sample(0)]),
            Err(MediaDensityError::Limit(LimitError::MediaDensityExceeded {
                unit: DensityUnit::Packets,
                limit: 2,
                observed: 3,
            }))
        );
    }

    #[test]
    fn fast_arrival_does_not_matter_when_media_density_is_valid() {
        let mut density = MediaDensityWindow::new(limits(), &timeline());

        for second in 0..10 {
            density
                .admit(1, &[sample(second)])
                .expect("one packet per media second is valid");
        }
    }
}
