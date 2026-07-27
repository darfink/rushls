//! Presentation, timeline, and sample fixtures.
//!
//! Builds on [`domain::fixtures`](crate::domain::fixtures) with the types this
//! layer introduces. Validation runs against
//! [`StreamPolicy::permissive`](crate::admission::StreamPolicy::permissive), so
//! a fixture only fails when the *track set* is genuinely inadmissible rather
//! than because some unrelated limit was left at a production default.

use crate::{
    admission::StreamPolicy,
    domain::{
        Codec, DiscoveredTrack, MediaKind, Payload, TickDuration, TickTimestamp, Timebase, TrackId,
        fixtures as domain,
    },
};

use super::{
    AudioSample, NormalizedSample, PresentationPlan, TimelineCalibration, TrackTimeline,
    VideoSample, validate,
};

/// Ticks in one second of the 90 kHz video timebase these fixtures use.
pub const VIDEO_SECOND: i64 = 90_000;

/// Ticks in one second of the 48 kHz audio timebase these fixtures use.
pub const AUDIO_SECOND: i64 = 48_000;

/// Validates a track set that is expected to be admissible.
pub fn presentation(tracks: Vec<DiscoveredTrack>) -> PresentationPlan {
    validate(&domain::catalog(tracks), &StreamPolicy::permissive())
        .expect("test presentation is valid")
}

/// The single-video-track presentation most pipeline tests are built on.
pub fn video_presentation() -> PresentationPlan {
    presentation(vec![domain::track(0, MediaKind::Video)])
}

/// Calibration placing each `(track id, timebase)` at origin zero.
///
/// The first entry is the timing authority, which matches what
/// [`calibrate`](super::calibrate) picks for a video-led presentation.
pub fn timeline(tracks: impl IntoIterator<Item = (u32, Timebase)>) -> TimelineCalibration {
    calibrated(
        tracks
            .into_iter()
            .map(|(track_id, timebase)| (track_id, timebase, 0)),
    )
}

/// As [`timeline`], with each track's shared origin expressed in its own ticks.
///
/// A zero origin hides the bug this exists to catch: two tracks whose origins
/// differ numerically but denote the same instant must still compare equal.
pub fn calibrated(
    tracks: impl IntoIterator<Item = (u32, Timebase, TickTimestamp)>,
) -> TimelineCalibration {
    let tracks: Vec<_> = tracks
        .into_iter()
        .map(|(track_id, timebase, origin_pts)| TrackTimeline {
            track_id: TrackId(track_id),
            timebase,
            origin_pts,
        })
        .collect();
    TimelineCalibration {
        timing_authority: tracks.first().expect("a timeline has a track").track_id,
        tracks,
    }
}

/// A single 90 kHz video track at origin zero.
pub fn video_timeline() -> TimelineCalibration {
    timeline([(0, Timebase::hz90k())])
}

/// One 90 kHz video access unit on track zero.
pub fn video_sample(
    pts: TickTimestamp,
    duration: TickDuration,
    random_access: bool,
    payload_bytes: usize,
) -> NormalizedSample {
    NormalizedSample::Video(VideoSample {
        track_id: TrackId(0),
        codec: Codec::H264,
        pts,
        dts: pts,
        duration,
        random_access,
        payload: Payload::from(vec![0; payload_bytes]),
    })
}

/// A keyframe at `second` carrying no payload.
///
/// Deliberately byte-free: pacing and density both judge a sample by its
/// timestamps, and giving it bytes would only invite a reader to think the
/// payload is what the test is about.
pub fn video_sample_at(second: i64) -> NormalizedSample {
    video_sample(second * VIDEO_SECOND, 3_000, true, 0)
}

/// One 48 kHz audio access unit carrying no payload.
pub fn audio_sample(track_id: u32, pts: TickTimestamp, duration: TickDuration) -> NormalizedSample {
    NormalizedSample::Audio(AudioSample {
        track_id: TrackId(track_id),
        codec: Codec::Aac,
        pts,
        duration,
        payload: Payload::default(),
    })
}
