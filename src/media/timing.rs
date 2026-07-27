use thiserror::Error;

use crate::domain::{
    DiscoveredTrack, MediaParameters, TickDuration, TickTimestamp, Timebase, TrackId,
};

use super::NormalizedSample;

/// The portion of an encoded access unit that belongs on the presentation
/// timeline after codec padding is removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentedTiming {
    pub start: TickTimestamp,
    pub duration: TickDuration,
}

impl PresentedTiming {
    pub fn end(self) -> Option<TickTimestamp> {
        self.start.checked_add_unsigned(self.duration)
    }
}

/// Computes effective presentation ranges in access-unit order.
///
/// FFmpeg attaches the complete leading skip count to one packet, even when
/// that count spans several decoded access units. Keeping the remainder here
/// lets timing consumers suppress those later units without rewriting the
/// packet-local [`crate::domain::AudioTrim`] that the muxer must pass back to FFmpeg.
///
/// The per-track constants are captured once, at construction. That is what
/// lets consumers hold a cursor rather than a whole [`DiscoveredTrack`], and it
/// retires the checks that used to re-establish on every access unit that the
/// supplied track was the cursor's track and carried audio parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentedTimingCursor {
    track_id: TrackId,
    /// Present only for audio: what a trim measured in decoded samples costs
    /// in this track's ticks. Absent for kinds that cannot be trimmed.
    audio: Option<AudioTrimScale>,
    pending_leading_audio_ticks: TickDuration,
}

/// Converts a decoded-sample trim into one track's tick domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioTrimScale {
    sample_rate: u32,
    timebase: Timebase,
}

impl AudioTrimScale {
    fn ticks(self, samples: u32) -> Result<TickDuration, SampleTimingError> {
        audio_samples_to_ticks_exact(samples, self.sample_rate, self.timebase)
    }
}

impl PresentedTimingCursor {
    /// Starts presentation accounting for one discovered track.
    pub fn for_track(track: &DiscoveredTrack) -> Self {
        Self {
            track_id: track.id,
            audio: match track.parameters {
                MediaParameters::Audio { sample_rate, .. } => Some(AudioTrimScale {
                    sample_rate: sample_rate.get(),
                    timebase: track.timebase,
                }),
                _ => None,
            },
            pending_leading_audio_ticks: 0,
        }
    }

    /// Returns the next access unit's effective range without changing the
    /// packet trim metadata retained on `sample`.
    ///
    /// Access units must arrive in the order the track produced them: the
    /// leading skip carried between calls is meaningless out of order.
    pub fn next(
        &mut self,
        sample: &NormalizedSample,
    ) -> Result<PresentedTiming, SampleTimingError> {
        if sample.track_id() != self.track_id {
            return Err(SampleTimingError::WrongTrack);
        }
        let (packet_leading, trailing) = match (sample, self.audio) {
            (NormalizedSample::Audio(sample), Some(scale)) => (
                scale.ticks(sample.trim.leading_samples)?,
                scale.ticks(sample.trim.trailing_samples)?,
            ),
            // An audio sample on a track discovered as another kind: its trim
            // has no scale to convert through, so its timing cannot be trusted.
            (NormalizedSample::Audio(_), None) => return Err(SampleTimingError::WrongTrack),
            _ => (0, 0),
        };
        let leading = self
            .pending_leading_audio_ticks
            .checked_add(packet_leading)
            .ok_or(SampleTimingError::AudioTrimOverflow)?;
        let applied_leading = leading.min(sample.duration());
        self.pending_leading_audio_ticks = leading - applied_leading;
        let duration = sample
            .duration()
            .checked_sub(applied_leading)
            .and_then(|duration| duration.checked_sub(trailing))
            .ok_or(SampleTimingError::TrimExceedsDuration)?;
        let start = sample
            .pts()
            .checked_add_unsigned(applied_leading)
            .ok_or(SampleTimingError::TimestampOverflow)?;
        Ok(PresentedTiming { start, duration })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SampleTimingError {
    #[error("sample belongs to a different track")]
    WrongTrack,
    #[error("audio trim cannot be represented exactly in the track timebase")]
    InexactAudioTrim,
    #[error("accumulated audio trim overflowed")]
    AudioTrimOverflow,
    #[error("audio trim exceeds the encoded access-unit duration")]
    TrimExceedsDuration,
    #[error("presented sample timestamp overflowed")]
    TimestampOverflow,
}

fn audio_samples_to_ticks_exact(
    samples: u32,
    sample_rate: u32,
    timebase: Timebase,
) -> Result<TickDuration, SampleTimingError> {
    let numerator = u128::from(samples)
        .checked_mul(u128::from(timebase.den().get()))
        .ok_or(SampleTimingError::InexactAudioTrim)?;
    let denominator = u128::from(sample_rate)
        .checked_mul(u128::from(timebase.num().get()))
        .ok_or(SampleTimingError::InexactAudioTrim)?;
    if !numerator.is_multiple_of(denominator) {
        return Err(SampleTimingError::InexactAudioTrim);
    }
    TickDuration::try_from(numerator / denominator).map_err(|_| SampleTimingError::InexactAudioTrim)
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{AudioTrim, Codec, MediaKind, Payload, fixtures::TrackBuilder},
        media::AudioSample,
    };

    use super::*;

    #[test]
    fn presented_audio_timing_excludes_priming_and_trailing_padding() {
        let track = TrackBuilder::new(1, MediaKind::Audio)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(48_000)))
            .build();
        let priming = NormalizedSample::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: -1_024,
            duration: 1_024,
            trim: AudioTrim {
                leading_samples: 1_024,
                trailing_samples: 0,
            },
            payload: Payload::default(),
        });
        let tail = NormalizedSample::Audio(AudioSample {
            track_id: TrackId(1),
            codec: Codec::Aac,
            pts: 0,
            duration: 1_024,
            trim: AudioTrim {
                leading_samples: 0,
                trailing_samples: 24,
            },
            payload: Payload::default(),
        });
        let mut timing = PresentedTimingCursor::for_track(&track);

        assert_eq!(
            timing.next(&priming),
            Ok(PresentedTiming {
                start: 0,
                duration: 0,
            })
        );
        assert_eq!(
            timing.next(&tail),
            Ok(PresentedTiming {
                start: 0,
                duration: 1_000,
            })
        );
    }

    #[test]
    fn leading_trim_spans_access_units_without_rewriting_packet_metadata() {
        let track = TrackBuilder::new(1, MediaKind::Audio)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(48_000)))
            .build();
        let original_trim = AudioTrim {
            leading_samples: 2_112,
            trailing_samples: 0,
        };
        let samples = [
            NormalizedSample::Audio(AudioSample {
                track_id: track.id,
                codec: Codec::Aac,
                pts: -2_112,
                duration: 1_024,
                trim: original_trim,
                payload: Payload::default(),
            }),
            NormalizedSample::Audio(AudioSample {
                track_id: track.id,
                codec: Codec::Aac,
                pts: -1_088,
                duration: 1_024,
                trim: AudioTrim::default(),
                payload: Payload::default(),
            }),
            NormalizedSample::Audio(AudioSample {
                track_id: track.id,
                codec: Codec::Aac,
                pts: -64,
                duration: 1_024,
                trim: AudioTrim::default(),
                payload: Payload::default(),
            }),
        ];
        let mut timing = PresentedTimingCursor::for_track(&track);

        assert_eq!(
            timing.next(&samples[0]),
            Ok(PresentedTiming {
                start: -1_088,
                duration: 0,
            })
        );
        assert_eq!(
            timing.next(&samples[1]),
            Ok(PresentedTiming {
                start: -64,
                duration: 0,
            })
        );
        assert_eq!(
            timing.next(&samples[2]),
            Ok(PresentedTiming {
                start: 0,
                duration: 960,
            })
        );
        assert_eq!(
            match &samples[0] {
                NormalizedSample::Audio(sample) => sample.trim,
                _ => unreachable!("fixture is audio"),
            },
            original_trim,
            "presentation accounting must not rewrite FFmpeg packet side data"
        );
    }
}
