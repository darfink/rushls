use thiserror::Error;

use std::time::Duration;

use crate::domain::{MediaInstant, MediaKind, TickTimestamp, Timebase, TrackId};

use super::PresentationPlan;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackTimeline {
    pub track_id: TrackId,
    /// Timebase of normalized access units produced for this track.
    pub timebase: Timebase,
    /// Track-local PTS corresponding to the shared presentation origin.
    pub origin_pts: TickTimestamp,
}

/// Track-local mappings onto one shared presentation origin.
///
/// Normalized access units and every downstream plan keep each track's declared
/// timebase; the authority's timebase is only used to compare start times.
///
/// This statement describes the calibration returned directly by
/// [`calibrate`]. A normalizer that changes representation projects both this
/// calibration and the validated presentation atomically before either reaches
/// segmentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineCalibration {
    pub timing_authority: TrackId,
    pub tracks: Vec<TrackTimeline>,
}

impl TrackTimeline {
    /// This track's PTS `duration` of presentation time past its origin.
    ///
    /// Rounding is the caller's to choose and it matters: a search *limit*
    /// rounds down so a selected boundary never exceeds the policy, while an
    /// observation *horizon* rounds up so pre-roll consumes at least through
    /// the duration before it concludes anything. Spelling both at every call
    /// site is what previously put this arithmetic in five places.
    pub fn horizon(&self, duration: Duration, rounding: Rounding) -> Option<TickTimestamp> {
        let ticks = match rounding {
            Rounding::Down => self.timebase.duration_to_ticks_floor(duration),
            Rounding::Up => self.timebase.duration_to_ticks_ceil(duration),
        };
        self.origin_pts.checked_add_unsigned(ticks)
    }

    /// Places a track-local timestamp on the shared presentation timeline.
    pub fn instant(&self, pts: TickTimestamp) -> MediaInstant {
        MediaInstant::new(self.timebase, pts, self.origin_pts)
    }
}

/// Which way a converted duration is rounded when it lands between ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rounding {
    Down,
    Up,
}

impl TimelineCalibration {
    pub fn get(&self, track_id: TrackId) -> Option<&TrackTimeline> {
        self.tracks.iter().find(|track| track.track_id == track_id)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TimelineCalibrationError {
    #[error("no suitable timeline authority was found")]
    NoTimelineAuthority,
    #[error("{track_id} provided no usable start timestamp")]
    NoUsableTimestamp { track_id: TrackId },
    #[error("the shared origin cannot be represented in the timebase of {track_id}")]
    InvalidTrackShift { track_id: TrackId },
}

/// Places every track on one shared presentation origin.
///
/// This runs on discovery metadata alone, so it is a planning step rather than
/// part of pre-roll: it needs no media to flow. The origin is the *earliest*
/// presented track start. Tracks that begin later retain a positive offset,
/// which a pass-through MP4 muxer represents with an empty edit instead of
/// clipping media from the earlier track.
pub fn calibrate(
    presentation: &PresentationPlan,
) -> Result<TimelineCalibration, TimelineCalibrationError> {
    let tracks = presentation.tracks();
    // Video anchors the comparison when present: its timebase is the one every
    // segment boundary is ultimately expressed against.
    let authority = tracks
        .iter()
        .find(|track| track.kind() == MediaKind::Video)
        .or_else(|| tracks.first())
        .ok_or(TimelineCalibrationError::NoTimelineAuthority)?;

    let mut shared_origin = TickTimestamp::MAX;
    for track in tracks {
        let first_pts = track
            .first_pts
            .ok_or(TimelineCalibrationError::NoUsableTimestamp { track_id: track.id })?;
        let in_authority_ticks = track
            .timebase
            .checked_rescale_ticks(first_pts, authority.timebase)
            .ok_or(TimelineCalibrationError::InvalidTrackShift { track_id: track.id })?;
        shared_origin = shared_origin.min(in_authority_ticks);
    }

    let mut calibrated = Vec::with_capacity(tracks.len());
    for track in tracks {
        let local = authority
            .timebase
            .checked_rescale_ticks(shared_origin, track.timebase)
            .ok_or(TimelineCalibrationError::InvalidTrackShift { track_id: track.id })?;

        calibrated.push(TrackTimeline {
            track_id: track.id,
            timebase: track.timebase,
            origin_pts: local,
        });
    }

    Ok(TimelineCalibration {
        timing_authority: authority.id,
        tracks: calibrated,
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{DiscoveredTrack, fixtures::TrackBuilder},
        media::fixtures::presentation,
    };

    use super::*;

    fn track(
        id: u32,
        kind: MediaKind,
        timebase: Timebase,
        first_pts: Option<TickTimestamp>,
    ) -> DiscoveredTrack {
        TrackBuilder::new(id, kind)
            .timebase(timebase)
            .first_pts(first_pts)
            .build()
    }

    #[test]
    fn the_shared_origin_is_the_earliest_track_start() {
        let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let plan = presentation(vec![
            track(0, MediaKind::Video, Timebase::hz90k(), Some(90_000)),
            track(1, MediaKind::Audio, audio_base, Some(3 * 48_000)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        assert_eq!(calibration.timing_authority, TrackId(0));
        // Video starts at 1s and audio at 3s, so both origins land on 1s. The
        // audio track retains a two-second positive presentation offset.
        assert_eq!(
            calibration.get(TrackId(0)).map(|track| track.origin_pts),
            Some(90_000)
        );
        assert_eq!(
            calibration.get(TrackId(1)).map(|track| track.origin_pts),
            Some(48_000)
        );
    }

    #[test]
    fn video_anchors_the_comparison_even_when_it_is_not_first() {
        let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let plan = presentation(vec![
            track(0, MediaKind::Audio, audio_base, Some(0)),
            track(1, MediaKind::Video, Timebase::hz90k(), Some(0)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        assert_eq!(calibration.timing_authority, TrackId(1));
    }

    #[test]
    fn genuine_earlier_audio_delays_video_from_the_shared_origin() {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(1_000));
        let plan = presentation(vec![
            track(0, MediaKind::Audio, timebase, Some(-22)),
            track(1, MediaKind::Video, timebase, Some(0)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        assert_eq!(
            calibration.get(TrackId(0)).map(|track| track.origin_pts),
            Some(-22)
        );
        assert_eq!(
            calibration.get(TrackId(1)).map(|track| track.origin_pts),
            Some(-22)
        );
    }

    #[test]
    fn codec_priming_does_not_move_the_declared_audible_origin() {
        let plan = presentation(vec![
            TrackBuilder::new(0, MediaKind::Audio)
                .first_pts(Some(0))
                .parameters(crate::domain::MediaParameters::Audio {
                    sample_rate: nz::u32!(48_000),
                    channels: nz::u16!(2),
                    frame_size: Some(nz::u32!(1_024)),
                    bit_depth: None,
                    timing: crate::domain::AudioTiming {
                        initial_padding_samples: 1_024,
                        ..crate::domain::AudioTiming::default()
                    },
                })
                .build(),
            track(1, MediaKind::Video, Timebase::hz90k(), Some(0)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        assert!(calibration.tracks.iter().all(|track| track.origin_pts == 0));
    }

    #[test]
    fn rebasing_preserves_the_interval_between_two_tracks() {
        // The invariant every consumer of `origin_pts` depends on: each track's
        // origin names *one* instant in that track's own ticks, so rebasing by
        // it shifts every track by the same amount of real time. A per-track
        // adjustment — snapping to a frame boundary, say — silently moves one
        // track relative to the other, which is an A/V sync error no test
        // downstream of here would attribute to calibration.
        let video_base = Timebase::hz90k();
        let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        // Video starts a second after audio, in each track's own domain.
        let plan = presentation(vec![
            track(0, MediaKind::Video, video_base, Some(3 * 90_000)),
            track(1, MediaKind::Audio, audio_base, Some(2 * 48_000)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");
        let video = calibration.get(TrackId(0)).expect("video is calibrated");
        let audio = calibration.get(TrackId(1)).expect("audio is calibrated");

        let video_start = video
            .instant(3 * 90_000)
            .elapsed_since(video.instant(video.origin_pts))
            .expect("video start is after its origin");
        let audio_start = audio
            .instant(2 * 48_000)
            .elapsed_since(audio.instant(audio.origin_pts))
            .expect("audio start is after its origin");

        assert_eq!(
            audio_start,
            Duration::ZERO,
            "the earlier track anchors zero"
        );
        assert_eq!(
            video_start - audio_start,
            Duration::from_secs(1),
            "the one-second gap between the tracks must survive rebasing"
        );
    }

    #[test]
    fn an_origin_may_precede_a_later_track_start() {
        let odd_base = Timebase::new(nz::u32!(1), nz::u32!(7));
        let plan = presentation(vec![
            track(0, MediaKind::Video, Timebase::hz90k(), Some(90_001)),
            track(1, MediaKind::Audio, odd_base, Some(8)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        assert_eq!(
            calibration.get(TrackId(0)).map(|track| track.origin_pts),
            Some(90_001)
        );
        assert_eq!(
            calibration.get(TrackId(1)).map(|track| track.origin_pts),
            Some(7)
        );
        assert!(
            calibration
                .get(TrackId(1))
                .is_some_and(|track| track.origin_pts < 8)
        );
    }

    #[test]
    fn a_track_without_a_start_timestamp_cannot_be_calibrated() {
        let plan = presentation(vec![
            track(0, MediaKind::Video, Timebase::hz90k(), Some(0)),
            track(1, MediaKind::Audio, Timebase::hz90k(), None),
        ]);

        assert_eq!(
            calibrate(&plan),
            Err(TimelineCalibrationError::NoUsableTimestamp {
                track_id: TrackId(1),
            })
        );
    }
}
