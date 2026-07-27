use std::time::Duration;

use crate::source::{DensityUnit, InputLimits, LimitError};

use super::{
    NormalizedSample, PacingError, TimelineCalibration,
    pacer::{MediaPoint, MediaWatermark},
};

/// Fixed media-time accounting for one publisher.
///
/// Unlike an arrival-rate limiter, this window advances only when normalized
/// media time advances. Replaying ten valid seconds quickly is therefore fine;
/// sending unbounded packets or samples at one timestamp is not.
pub struct MediaDensityWindow {
    limits: InputLimits,
    timeline: TimelineCalibration,
    window_start: Option<MediaPoint>,
    watermark: MediaWatermark,
    bytes: u64,
    packets: u64,
    samples: u64,
}

impl MediaDensityWindow {
    pub fn new(limits: InputLimits, timeline: &TimelineCalibration) -> Self {
        Self {
            limits,
            timeline: timeline.clone(),
            window_start: None,
            watermark: MediaWatermark::default(),
            bytes: 0,
            packets: 0,
            samples: 0,
        }
    }

    pub fn admit(
        &mut self,
        bytes: u64,
        packets: u64,
        samples: &[NormalizedSample],
    ) -> Result<(), MediaDensityError> {
        for sample in samples {
            self.watermark.observe(sample, &self.timeline)?;
        }

        if let Some(current) = self.watermark.get() {
            match self.window_start {
                None => self.window_start = Some(current),
                Some(start)
                    if current.elapsed_since(start)? >= self.limits.media_density_window =>
                {
                    self.window_start = Some(current);
                    self.bytes = 0;
                    self.packets = 0;
                    self.samples = 0;
                }
                Some(_) => {}
            }
        }

        self.bytes = self.bytes.saturating_add(bytes);
        self.packets = self.packets.saturating_add(packets);
        self.samples = self
            .samples
            .saturating_add(u64::try_from(samples.len()).unwrap_or(u64::MAX));

        check(
            self.bytes,
            self.limits.maximum_bytes_per_media_second,
            self.limits.media_density_window,
            DensityUnit::Bytes,
        )?;
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

#[derive(Clone, Debug, thiserror::Error, Eq, PartialEq)]
pub enum MediaDensityError {
    #[error(transparent)]
    Timestamp(#[from] PacingError),
    #[error(transparent)]
    Limit(#[from] LimitError),
}

#[cfg(test)]
mod tests {
    use crate::media::fixtures::{video_sample_at as sample, video_timeline as timeline};

    use super::*;

    fn limits() -> InputLimits {
        InputLimits {
            maximum_bytes_per_media_second: 1_000,
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
            .admit(10, 2, &[sample(0)])
            .expect("the first media window is within its cap");

        assert_eq!(
            density.admit(10, 1, &[sample(0)]),
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
                .admit(10, 1, &[sample(second)])
                .expect("one packet per media second is valid");
        }
    }
}
