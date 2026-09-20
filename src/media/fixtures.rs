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
        AudioTrim, Codec, DiscoveredTrack, MediaKind, Payload, TickDuration, TickTimestamp,
        Timebase, TrackId, fixtures as domain,
    },
};

use super::{
    AudioSample, NormalizedMedia, PresentationPlan, TimelineCalibration, TrackTimeline,
    VideoSample, validate,
};

/// Ticks in one second of the 90 kHz video timebase these fixtures use.
pub const VIDEO_SECOND: i64 = 90_000;

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
) -> NormalizedMedia {
    NormalizedMedia::Video(VideoSample {
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
pub fn video_sample_at(second: i64) -> NormalizedMedia {
    video_sample(second * VIDEO_SECOND, 3_000, true, 0)
}

/// One 48 kHz audio access unit carrying no payload.
pub fn audio_sample(track_id: u32, pts: TickTimestamp, duration: TickDuration) -> NormalizedMedia {
    NormalizedMedia::Audio(AudioSample {
        track_id: TrackId(track_id),
        codec: Codec::Aac,
        pts,
        duration,
        trim: AudioTrim::default(),
        payload: Payload::default(),
    })
}

// FFmpeg-generated 64x64 25fps headers; see cadence tests for encoder options.
pub const H264_FIXED_CADENCE: &[u8] = &[
    0x01, 0x64, 0x00, 0x0a, 0xff, 0xe1, 0x00, 0x18, 0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x44, 0x26,
    0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xca, 0x3c, 0x48, 0x96, 0x58,
    0x01, 0x00, 0x06, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0, 0xfd, 0xf8, 0xf8, 0x00,
];
pub const HEVC_FIXED_CADENCE: &[u8] = &[
    0x01, 0x01, 0x60, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3c, 0xf0, 0x00, 0xfc,
    0xfd, 0xf8, 0xf8, 0x00, 0x00, 0x0f, 0x03, 0x20, 0x00, 0x01, 0x00, 0x18, 0x40, 0x01, 0x0c, 0x01,
    0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
    0x3c, 0x92, 0x80, 0x90, 0x21, 0x00, 0x01, 0x00, 0x33, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00,
    0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x3c, 0xa0, 0x20, 0x81, 0x05, 0x96,
    0x4a, 0x92, 0x4c, 0xaf, 0x01, 0x68, 0x08, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x00,
    0xcb, 0x00, 0xa4, 0xb2, 0xf0, 0x00, 0x7a, 0x12, 0x00, 0x0f, 0x42, 0x44, 0x22, 0x00, 0x01, 0x00,
    0x07, 0x44, 0x01, 0xc1, 0x72, 0xb4, 0x22, 0x40,
];
pub const AV1_FIXED_CADENCE: &[u8] = &[
    0x81, 0x00, 0x0c, 0x00, 0x0a, 0x13, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x67, 0x40,
    0x00, 0x00, 0xa2, 0xaf, 0xff, 0x80, 0x5f, 0x00, 0x08,
];

// FFmpeg color=black:size=64x64:rate=25:duration=0.16, libx264
// ultrafast, bf=0, force-cfr=1:scenecut=0, FLV. Encoder SEI omitted.
// Includes sequence header and four coded pictures, for real RTMP parsing.
pub const H264_CFR_FLV_UNITS: &[(u32, &[u8])] = &[
    (
        0,
        &[
            0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0xc0, 0x0a, 0xff, 0xe1, 0x00, 0x16, 0x67,
            0x42, 0xc0, 0x0a, 0xda, 0x10, 0x9b, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00,
            0x00, 0x03, 0x03, 0x28, 0xf1, 0x22, 0x6a, 0x01, 0x00, 0x04, 0x68, 0xce, 0x0f, 0xc8,
        ],
    ),
    (
        0,
        &[
            0x17, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x16, 0x65, 0x88, 0x84, 0x3a, 0x26,
            0x28, 0x00, 0x09, 0x02, 0xc9, 0xc9, 0xc9, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7,
            0x5d, 0x75, 0xe0,
        ],
    ),
    (
        40,
        &[
            0x27, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x41, 0x9a, 0x20, 0x3a, 0x82,
            0x30,
        ],
    ),
    (
        80,
        &[
            0x27, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x41, 0x9a, 0x40, 0x3e, 0x82,
            0x30,
        ],
    ),
    (
        120,
        &[
            0x27, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x41, 0x9a, 0x60, 0x3e, 0x82,
            0x30,
        ],
    ),
];
