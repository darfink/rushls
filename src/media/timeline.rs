use thiserror::Error;

use crate::domain::{MediaKind, TickTimestamp, Timebase, TrackId};

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineCalibration {
    pub timing_authority: TrackId,
    pub tracks: Vec<TrackTimeline>,
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
/// part of pre-roll: it needs no media to flow. The origin is the *latest*
/// track start, because starting earlier would ask a track for media it has not
/// produced. Each track's origin is then expressed in its own tick domain and
/// never allowed to precede that track's first sample.
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

    let mut shared_origin = TickTimestamp::MIN;
    for track in tracks {
        let first_pts = track
            .first_pts
            .ok_or(TimelineCalibrationError::NoUsableTimestamp { track_id: track.id })?;
        let in_authority_ticks = track.timebase.rescale_ticks(first_pts, authority.timebase);
        shared_origin = shared_origin.max(in_authority_ticks);
    }

    let mut calibrated = Vec::with_capacity(tracks.len());
    for track in tracks {
        let first_pts = track
            .first_pts
            .ok_or(TimelineCalibrationError::NoUsableTimestamp { track_id: track.id })?;
        let local = authority
            .timebase
            .rescale_ticks(shared_origin, track.timebase);
        if local == TickTimestamp::MIN || local == TickTimestamp::MAX {
            return Err(TimelineCalibrationError::InvalidTrackShift { track_id: track.id });
        }

        calibrated.push(TrackTimeline {
            track_id: track.id,
            timebase: track.timebase,
            // Rescaling rounds, so clamp forward rather than let an origin land
            // a tick before the first sample it is supposed to name.
            origin_pts: local.max(first_pts),
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
    fn the_shared_origin_is_the_latest_track_start() {
        let audio_base = Timebase::new(nz::u32!(1), nz::u32!(48_000));
        let plan = presentation(vec![
            track(0, MediaKind::Video, Timebase::hz90k(), Some(90_000)),
            track(1, MediaKind::Audio, audio_base, Some(3 * 48_000)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        assert_eq!(calibration.timing_authority, TrackId(0));
        // Audio starts at 3s and video at 1s, so both origins land on 3s.
        assert_eq!(
            calibration.get(TrackId(0)).map(|track| track.origin_pts),
            Some(3 * 90_000)
        );
        assert_eq!(
            calibration.get(TrackId(1)).map(|track| track.origin_pts),
            Some(3 * 48_000)
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
    fn an_origin_never_precedes_the_track_it_names() {
        let odd_base = Timebase::new(nz::u32!(1), nz::u32!(7));
        let plan = presentation(vec![
            track(0, MediaKind::Video, Timebase::hz90k(), Some(90_001)),
            track(1, MediaKind::Audio, odd_base, Some(8)),
        ]);

        let calibration = calibrate(&plan).expect("calibration succeeds");

        for track in &calibration.tracks {
            let first_pts = plan
                .tracks()
                .iter()
                .find(|candidate| candidate.id == track.track_id)
                .and_then(|candidate| candidate.first_pts)
                .expect("test track declares a start");
            assert!(track.origin_pts >= first_pts);
        }
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
