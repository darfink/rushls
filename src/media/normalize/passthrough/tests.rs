use super::recovery::AudioRecoveryPolicy;
use crate::{
    domain::{
        AudioTiming, AudioTrim, Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters,
        Payload, TickDuration, Timebase, TrackId, fixtures::TrackBuilder,
    },
    media::{
        NormalizeError, NormalizedMedia, NormalizerFactory, PresentationPlan, StartedNormalizer,
        SubtitleSample, TimelineCalibration, calibrate, fixtures::presentation,
    },
    source::Packet,
};

use super::PassThroughNormalizerFactory;

fn packet(track_id: u32, pts: Option<i64>, dts: Option<i64>, duration: Option<i64>) -> Packet {
    Packet {
        track_id: TrackId(track_id),
        pts,
        dts,
        duration,
        random_access: false,
        audio_trim: AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: Payload::default(),
    }
}

fn start(
    tracks: Vec<DiscoveredTrack>,
) -> (StartedNormalizer, PresentationPlan, TimelineCalibration) {
    let input = presentation(tracks);
    started_from(input)
}

fn started_from(
    input: PresentationPlan,
) -> (StartedNormalizer, PresentationPlan, TimelineCalibration) {
    let timeline = calibrate(&input).expect("input timeline calibrates");
    let started = PassThroughNormalizerFactory
        .start(&input, &timeline, crate::domain::InputMode::Permissive)
        .expect("normalization starts");
    (started, input, timeline)
}

#[test]
fn startup_projects_track_metadata_and_the_shared_origin_together() {
    let audio = TrackBuilder::new(0, MediaKind::Audio)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .first_pts(Some(-22))
        .build();
    let video = TrackBuilder::new(1, MediaKind::Video)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(10_000)))
        .first_pts(Some(0))
        .build();

    let (started, _, _) = start(vec![audio, video]);
    let audio = started
        .presentation
        .catalog()
        .get(TrackId(0))
        .expect("audio track exists");
    let video = started
        .presentation
        .catalog()
        .get(TrackId(1))
        .expect("video track exists");

    assert_eq!(
        (audio.timebase, audio.first_pts),
        (Timebase::new(nz::u32!(1), nz::u32!(48_000)), Some(-1_056))
    );
    assert_eq!(
        (video.timebase, video.first_pts),
        (Timebase::hz90k(), Some(0))
    );
    assert_eq!(
        started
            .timeline
            .get(TrackId(0))
            .map(|track| track.origin_pts),
        Some(-1_056)
    );
    assert_eq!(
        started
            .timeline
            .get(TrackId(1))
            .map(|track| track.origin_pts),
        Some(-1_980)
    );
}

fn primed_audio() -> DiscoveredTrack {
    TrackBuilder::new(0, MediaKind::Audio)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .first_pts(Some(0))
        .parameters(MediaParameters::Audio {
            sample_rate: nz::u32!(48_000),
            channels: nz::u16!(2),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: None,
            timing: AudioTiming {
                initial_padding_samples: 1_024,
                ..AudioTiming::default()
            },
        })
        .build()
}

fn millisecond_aac() -> DiscoveredTrack {
    TrackBuilder::new(0, MediaKind::Audio)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .first_pts(Some(0))
        .parameters(MediaParameters::Audio {
            sample_rate: nz::u32!(44_100),
            channels: nz::u16!(2),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: None,
            timing: AudioTiming::default(),
        })
        .build()
}

#[test]
fn audio_clock_preserves_exact_priming_despite_coarse_container_timestamps() {
    let (mut started, _, _) = start(vec![primed_audio()]);
    let mut first = packet(0, Some(-21), Some(-21), Some(21));
    first.audio_trim.leading_samples = 1_024;
    let second = packet(0, Some(0), Some(0), Some(21));
    let mut output = Vec::new();

    started
        .normalizer
        .push(first, &mut output)
        .expect("priming packet normalizes");
    started
        .normalizer
        .push(second, &mut output)
        .expect("audible packet normalizes");

    let samples: Vec<_> = output
        .iter()
        .map(|sample| match sample {
            NormalizedMedia::Audio(sample) => {
                (sample.pts, sample.duration, sample.trim.leading_samples)
            }
            _ => unreachable!("audio input only produces audio"),
        })
        .collect();
    assert_eq!(samples, [(-1_024, 1_024, 1_024), (0, 1_024, 0)]);
}

#[test]
fn first_packet_padding_must_agree_with_track_padding() {
    let (mut started, _, _) = start(vec![primed_audio()]);
    let mut first = packet(0, Some(-21), Some(-21), Some(21));
    first.audio_trim.leading_samples = 2_112;

    let error = started
        .normalizer
        .push(first, &mut Vec::new())
        .expect_err("conflicting demuxer metadata is rejected");

    assert!(error.to_string().contains("2112"));
    assert!(error.to_string().contains("1024"));
}

#[test]
fn audio_clock_rejects_a_real_timestamp_discontinuity() {
    let audio = TrackBuilder::new(0, MediaKind::Audio)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .parameters(MediaParameters::Audio {
            sample_rate: nz::u32!(48_000),
            channels: nz::u16!(2),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: None,
            timing: AudioTiming::default(),
        })
        .build();
    let (mut started, _, _) = start(vec![audio]);
    started
        .normalizer
        .configure_audio_recovery(AudioRecoveryPolicy {
            enabled: false,
            ..Default::default()
        })
        .expect("strict fixture policy");
    let mut output = Vec::new();
    started
        .normalizer
        .push(packet(0, Some(0), Some(0), Some(21)), &mut output)
        .expect("first packet anchors the clock");

    let error = started
        .normalizer
        .push(packet(0, Some(100), Some(100), Some(21)), &mut output)
        .expect_err("a 100 ms jump is not timestamp quantization");
    assert!(error.to_string().contains("audio_gap"));
}

#[test]
fn a_shortened_terminal_audio_packet_becomes_trailing_trim() -> Result<(), NormalizeError> {
    let (mut started, _, _) = start(vec![millisecond_aac()]);
    let mut output = Vec::new();

    started
        .normalizer
        .push(packet(0, Some(0), Some(0), Some(23)), &mut output)?;
    started
        .normalizer
        .push(packet(0, Some(23), Some(23), Some(17)), &mut output)?;

    assert_eq!(output.len(), 1, "the possible tail waits for end of input");
    started.normalizer.finish(&mut output)?;
    started.normalizer.finish(&mut output)?;

    assert_eq!(output.len(), 2, "finishing twice releases the tail once");
    assert!(matches!(
        &output[1],
        NormalizedMedia::Audio(sample)
            if sample.pts == 1_024
                && sample.duration == 1_024
                && sample.trim.trailing_samples == 274
    ));
    Ok(())
}

#[test]
fn a_shortened_audio_packet_with_a_successor_is_rejected() -> Result<(), NormalizeError> {
    let (mut started, _, _) = start(vec![millisecond_aac()]);
    let mut output = Vec::new();
    started
        .normalizer
        .push(packet(0, Some(0), Some(0), Some(23)), &mut output)?;
    started
        .normalizer
        .push(packet(0, Some(23), Some(23), Some(17)), &mut output)?;

    let error = started
        .normalizer
        .push(packet(0, Some(40), Some(40), Some(23)), &mut output)
        .expect_err("a non-terminal short frame is invalid");
    assert!(
        error
            .to_string()
            .contains("shortened audio packet was followed by more audio")
    );

    started.normalizer.finish(&mut output)?;
    assert_eq!(output.len(), 1, "the invalid candidate is never emitted");
    Ok(())
}

#[test]
fn explicit_terminal_trim_must_agree_with_the_short_duration() -> Result<(), NormalizeError> {
    let (mut agreeing, _, _) = start(vec![millisecond_aac()]);
    let mut packet_with_trim = packet(0, Some(0), Some(0), Some(17));
    packet_with_trim.audio_trim.trailing_samples = 280;
    let mut output = Vec::new();
    agreeing.normalizer.push(packet_with_trim, &mut output)?;
    agreeing.normalizer.finish(&mut output)?;
    assert!(matches!(
        &output[0],
        NormalizedMedia::Audio(sample) if sample.trim.trailing_samples == 280
    ));

    let (mut conflicting, _, _) = start(vec![millisecond_aac()]);
    let mut packet_with_trim = packet(0, Some(0), Some(0), Some(17));
    packet_with_trim.audio_trim.trailing_samples = 100;
    let error = conflicting
        .normalizer
        .push(packet_with_trim, &mut Vec::new())
        .expect_err("contradictory trim is rejected");
    assert!(error.to_string().contains("terminal audio trim disagrees"));
    Ok(())
}

#[test]
fn an_audio_packet_longer_than_its_fixed_frame_is_still_rejected() {
    let (mut started, _, _) = start(vec![millisecond_aac()]);

    let error = started
        .normalizer
        .push(packet(0, Some(0), Some(0), Some(30)), &mut Vec::new())
        .expect_err("an oversized fixed audio frame is invalid");

    assert!(
        error
            .to_string()
            .contains("audio packet duration disagrees")
    );
}

#[test]
fn av1_without_declared_timing_uses_observed_steps_and_rejects_frozen_timestamps()
-> Result<(), NormalizeError> {
    let video = TrackBuilder::new(0, MediaKind::Video)
        .codec(Codec::Av1)
        .parameters(MediaParameters::Video {
            width: nz::u32!(160),
            height: nz::u32!(96),
            frame_rate: None,
            video_delay: 0,
        })
        .build();
    let (mut normal, _, _) = start(vec![video.clone()]);
    let mut samples = Vec::new();
    for pts in [0, 9000, 18000] {
        normal
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut samples)?;
    }
    normal.normalizer.finish(&mut samples)?;
    assert_eq!(samples.len(), 3);
    assert!(samples.iter().all(|sample| sample.duration() == 9000));
    let (mut frozen, _, _) = start(vec![video]);
    frozen
        .normalizer
        .push(packet(0, Some(0), Some(0), None), &mut Vec::new())?;
    let error = frozen
        .normalizer
        .push(packet(0, Some(0), Some(0), None), &mut Vec::new())
        .expect_err("frozen DTS must fail before duration inference");
    assert!(error.to_string().contains("video_timestamp_order"));
    Ok(())
}

#[test]
fn video_derives_variable_durations_and_synthesizes_missing_dts() {
    let video = TrackBuilder::new(0, MediaKind::Video)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(10_000)))
        .parameters(MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: None,
            video_delay: 0,
        })
        .build();
    let (mut started, _, _) = start(vec![video]);
    let mut output = Vec::new();
    for pts in [0, 333, 667] {
        started
            .normalizer
            .push(packet(0, Some(pts), None, None), &mut output)
            .expect("video packet is accepted");
    }
    started
        .normalizer
        .finish(&mut output)
        .expect("video tail flushes");

    let timing: Vec<_> = output
        .iter()
        .map(|sample| match sample {
            NormalizedMedia::Video(sample) => (sample.pts, sample.dts, sample.duration),
            _ => unreachable!("video input only produces video"),
        })
        .collect();
    assert_eq!(
        timing,
        [(0, 0, 2_997), (2_997, 2_997, 3_006), (6_003, 6_003, 3_006)]
    );
}

/// FLV timestamps are milliseconds, so 24 fps arrives as a 42/41 ms cadence
/// while FFmpeg synthesizes a flat 41 ms `duration` from the declared frame
/// rate. Trusting that duration leaves every access unit 0.67 ms short of the
/// next DTS, which used to read as a decode discontinuity on the second frame.
#[test]
fn video_duration_follows_dts_steps_rather_than_a_quantized_packet_duration() {
    let video = millisecond_video(24, 0);
    let (mut started, _, _) = start(vec![video]);
    let mut output = Vec::new();
    for timestamp in [21, 63, 104, 146] {
        started
            .normalizer
            .push(
                packet(0, Some(timestamp), Some(timestamp), Some(41)),
                &mut output,
            )
            .expect("a millisecond-quantized step is not a discontinuity");
    }
    started
        .normalizer
        .finish(&mut output)
        .expect("video tail flushes");

    assert_eq!(
        video_timing(&output),
        [
            (1_890, 1_890, 3_780),
            (5_670, 5_670, 3_690),
            (9_360, 9_360, 3_780),
            // No successor times the tail, so the declared duration stands.
            (13_140, 13_140, 3_690)
        ]
    );
}

/// Without reordering, decode order is presentation order, so a PTS step
/// describes the decode timeline and outranks the same quantized duration.
#[test]
fn video_duration_follows_pts_steps_when_decode_order_is_presentation_order() {
    let video = millisecond_video(24, 0);
    let (mut started, _, _) = start(vec![video]);
    let mut output = Vec::new();
    for pts in [21, 63, 104, 146] {
        started
            .normalizer
            .push(packet(0, Some(pts), None, Some(41)), &mut output)
            .expect("a millisecond-quantized step is not a discontinuity");
    }
    started
        .normalizer
        .finish(&mut output)
        .expect("video tail flushes");

    assert_eq!(
        video_timing(&output),
        [
            (1_890, 1_890, 3_780),
            (5_670, 5_670, 3_690),
            (9_360, 9_360, 3_780),
            (13_140, 13_140, 3_690)
        ]
    );
}

fn millisecond_video(frame_rate: u32, video_delay: u32) -> DiscoveredTrack {
    TrackBuilder::new(0, MediaKind::Video)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .parameters(MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(
                frame_rate.try_into().expect("frame rate is positive"),
                nz::u32!(1),
            )),
            video_delay,
        })
        .build()
}

fn video_timing(output: &[NormalizedMedia]) -> Vec<(i64, i64, TickDuration)> {
    output
        .iter()
        .map(|sample| match sample {
            NormalizedMedia::Video(sample) => (sample.pts, sample.dts, sample.duration),
            _ => unreachable!("video input only produces video"),
        })
        .collect()
}

#[test]
fn reordered_video_uses_declared_delay_to_synthesize_negative_dts() {
    let video = TrackBuilder::new(0, MediaKind::Video)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .parameters(MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(nz::u32!(25), nz::u32!(1))),
            video_delay: 2,
        })
        .build();
    let (mut started, _, _) = start(vec![video]);
    let mut output = Vec::new();
    for pts in [0, 80, 40, 120] {
        started
            .normalizer
            .push(packet(0, Some(pts), None, None), &mut output)
            .expect("reordered packet is accepted");
    }
    started
        .normalizer
        .finish(&mut output)
        .expect("reordered tail flushes");

    let timing: Vec<_> = output
        .iter()
        .map(|sample| match sample {
            NormalizedMedia::Video(sample) => (sample.pts, sample.dts, sample.duration),
            _ => unreachable!("video input only produces video"),
        })
        .collect();
    assert_eq!(
        timing,
        [
            (0, -7_200, 3_600),
            (7_200, -3_600, 3_600),
            (3_600, 0, 3_600),
            (10_800, 3_600, 3_600)
        ]
    );
}

#[test]
fn fractional_declared_video_cadence_does_not_accumulate_rounding_drift() {
    let video = TrackBuilder::new(0, MediaKind::Video)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .parameters(MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(nz::u32!(24_000), nz::u32!(1_001))),
            video_delay: 2,
        })
        .build();
    let (mut started, _, _) = start(vec![video]);
    let mut output = Vec::new();
    for pts in [0, 83, 42, 125, 208, 167, 250, 292] {
        started
            .normalizer
            .push(packet(0, Some(pts), None, None), &mut output)
            .expect("fractional-cadence packet is accepted");
    }
    started
        .normalizer
        .finish(&mut output)
        .expect("fractional-cadence tail flushes");

    let video: Vec<_> = output
        .iter()
        .map(|sample| match sample {
            NormalizedMedia::Video(sample) => sample,
            _ => unreachable!("video input only produces video"),
        })
        .collect();
    assert_eq!(
        video
            .iter()
            .map(|sample| sample.duration)
            .sum::<TickDuration>(),
        30_030,
        "eight 24000/1001 frames are exactly 30030 ticks at 90 kHz"
    );
    assert!(
        video
            .windows(2)
            .all(|pair| pair[0].dts.checked_add_unsigned(pair[0].duration) == Some(pair[1].dts))
    );
}

#[test]
fn subtitles_reuse_interval_rescaling_for_start_and_duration() {
    let subtitle = TrackBuilder::new(0, MediaKind::Subtitle)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(10_000)))
        .build();
    // Validation requires a presentable audio or video track; the
    // normalizer still routes each track independently once admitted.
    let video = TrackBuilder::new(1, MediaKind::Video).build();
    let (mut started, _, _) = start(vec![subtitle, video]);
    let mut output = Vec::new();
    assert_eq!(
        started
            .presentation
            .catalog()
            .get(TrackId(0))
            .map(|track| track.timebase),
        Some(Timebase::hz90k())
    );

    started
        .normalizer
        .push(packet(0, Some(12_345), None, Some(6_789)), &mut output)
        .expect("subtitle cue normalizes");

    assert_eq!(
        output,
        [NormalizedMedia::Subtitle(SubtitleSample {
            track_id: TrackId(0),
            codec: Codec::WebVtt,
            pts: 111_105,
            duration: 61_101,
            webvtt: crate::domain::WebVttCueMetadata::default(),
            position: None,
            payload: Payload::default(),
        })]
    );
}

#[test]
fn subtitle_normalization_preserves_codec_specific_metadata() {
    let subtitle = TrackBuilder::new(0, MediaKind::Subtitle)
        .codec(Codec::SubRip)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .build();
    let video = TrackBuilder::new(1, MediaKind::Video).build();
    let (mut started, _, _) = start(vec![subtitle, video]);
    let mut input = packet(0, Some(1_000), None, Some(1_500));
    input.subtitle_position = Some(crate::domain::SubtitlePosition {
        x1: 10,
        y1: 20,
        x2: 100,
        y2: 80,
    });
    input.payload = Payload::from(b"subtitle".as_slice().to_vec());
    let mut output = Vec::new();

    started
        .normalizer
        .push(input, &mut output)
        .expect("SubRip cue normalizes");

    assert_eq!(
        output,
        [NormalizedMedia::Subtitle(SubtitleSample {
            track_id: TrackId(0),
            codec: Codec::SubRip,
            pts: 90_000,
            duration: 135_000,
            webvtt: crate::domain::WebVttCueMetadata::default(),
            position: Some(crate::domain::SubtitlePosition {
                x1: 10,
                y1: 20,
                x2: 100,
                y2: 80,
            }),
            payload: Payload::from(b"subtitle".as_slice().to_vec()),
        })]
    );
}

#[test]
fn an_open_ended_cue_may_arrive_without_a_duration() {
    // FLV script-data captions carry only the instant a cue becomes visible.
    // Zero travels on to the muxer, which resolves the end from the successor.
    let subtitle = TrackBuilder::new(0, MediaKind::Subtitle)
        .codec(Codec::Text)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
        .build();
    let video = TrackBuilder::new(1, MediaKind::Video).build();
    let (mut started, _, _) = start(vec![subtitle, video]);
    let mut output = Vec::new();

    for duration in [None, Some(0)] {
        output.clear();
        started
            .normalizer
            .push(packet(0, Some(1_000), None, duration), &mut output)
            .expect("an open-ended cue normalizes without a duration");

        let [NormalizedMedia::Subtitle(cue)] = output.as_slice() else {
            panic!("expected one subtitle cue, got {output:?}");
        };
        assert_eq!(cue.pts, 90_000);
        assert_eq!(cue.duration, 0);
        assert_eq!(cue.codec, Codec::Text);
    }
}

#[test]
fn a_closed_subtitle_codec_still_requires_a_positive_duration() {
    // Only the open-ended codecs may omit it: for these, a missing duration is
    // a demuxer or publisher fault rather than the shape of the format.
    for codec in [Codec::WebVtt, Codec::SubRip] {
        let subtitle = TrackBuilder::new(0, MediaKind::Subtitle)
            .codec(codec)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(1_000)))
            .build();
        let video = TrackBuilder::new(1, MediaKind::Video).build();
        let (mut started, _, _) = start(vec![subtitle, video]);
        let mut output = Vec::new();

        assert!(
            started
                .normalizer
                .push(packet(0, Some(1_000), None, Some(0)), &mut output)
                .is_err()
        );
    }
}

#[test]
fn finish_is_idempotent_after_releasing_a_held_video_packet() {
    let (mut started, _, _) = start(vec![TrackBuilder::new(0, MediaKind::Video).build()]);
    let mut output = Vec::new();
    started
        .normalizer
        .push(packet(0, Some(0), None, Some(3_000)), &mut output)
        .expect("packet is held");

    started
        .normalizer
        .finish(&mut output)
        .expect("first finish flushes");
    started
        .normalizer
        .finish(&mut output)
        .expect("second finish is harmless");

    assert_eq!(output.len(), 1);
}

#[test]
fn audio_backward_timestamps_respect_the_sample_clock_tolerance() -> Result<(), NormalizeError> {
    // A 48 kHz source clock gives an exact one-sample rounding tolerance.
    for field in ["PTS", "DTS"] {
        for lag in [1, 2, 1_025] {
            let audio = TrackBuilder::new(0, MediaKind::Audio)
                .timebase(Timebase::new(nz::u32!(1), nz::u32!(48_000)))
                .build();
            let (mut started, _, _) = start(vec![audio]);
            let mut output = Vec::new();
            started
                .normalizer
                .push(packet(0, Some(0), Some(0), Some(1_024)), &mut output)?;
            let supplied = 1_024 - lag;
            let (pts, dts) = if field == "PTS" {
                (supplied, 1_024)
            } else {
                (1_024, supplied)
            };
            let result = started
                .normalizer
                .push(packet(0, Some(pts), Some(dts), Some(1_024)), &mut output);
            if lag == 1 {
                result?;
                assert_eq!(output[1].pts(), 1_024);
            } else {
                let error = result.expect_err("backward timestamps outside tolerance must fail");
                assert!(error.to_string().contains(if field == "PTS" {
                    "timestamp_overlap"
                } else {
                    "audio_dts_mismatch"
                }));
                assert_eq!(output.len(), 1, "invalid audio must not be emitted");
            }
        }
    }
    Ok(())
}

#[test]
fn video_rejects_backward_and_duplicate_dts_before_holding_the_packet() -> Result<(), NormalizeError>
{
    for video_delay in [0, 2] {
        for invalid_dts in [-40, 0] {
            for flush in [false, true] {
                let (mut started, _, _) = start(vec![millisecond_video(25, video_delay)]);
                let mut output = Vec::new();
                started
                    .normalizer
                    .push(packet(0, Some(0), Some(0), Some(40)), &mut output)?;
                let error = started
                    .normalizer
                    .push(
                        packet(0, Some(40), Some(invalid_dts), Some(40)),
                        &mut output,
                    )
                    .expect_err("invalid DTS must fail before duration fallback or buffering");
                assert!(error.to_string().contains("video_timestamp_order"));
                if flush {
                    started.normalizer.finish(&mut output)?;
                } else {
                    started
                        .normalizer
                        .push(packet(0, Some(80), Some(80), Some(40)), &mut output)?;
                }
                assert!(
                    output.iter().all(|sample| sample.pts() == 0),
                    "invalid packet must never be emitted"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn non_reordered_video_rejects_backward_and_duplicate_pts() -> Result<(), NormalizeError> {
    for explicit_dts in [false, true] {
        for invalid_pts in [-40, 0] {
            let (mut started, _, _) = start(vec![millisecond_video(25, 0)]);
            let mut output = Vec::new();
            started.normalizer.push(
                packet(0, Some(0), explicit_dts.then_some(0), Some(40)),
                &mut output,
            )?;
            let error = started
                .normalizer
                .push(
                    packet(0, Some(invalid_pts), explicit_dts.then_some(40), Some(40)),
                    &mut output,
                )
                .expect_err("non-reordered PTS must increase even with valid DTS");
            assert!(error.to_string().contains("video_timestamp_order"));
            started.normalizer.finish(&mut output)?;
            assert_eq!(video_timing(&output), [(0, 0, 3_600)]);
        }
    }
    Ok(())
}

#[test]
fn reordered_video_accepts_backward_pts_with_increasing_explicit_dts() -> Result<(), NormalizeError>
{
    let (mut started, _, _) = start(vec![millisecond_video(25, 2)]);
    let mut output = Vec::new();
    for (pts, dts) in [(0, -80), (80, -40), (40, 0), (120, 40)] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(dts), Some(40)), &mut output)?;
    }
    started.normalizer.finish(&mut output)?;
    assert_eq!(
        video_timing(&output),
        [
            (0, -7_200, 3_600),
            (7_200, -3_600, 3_600),
            (3_600, 0, 3_600),
            (10_800, 3_600, 3_600)
        ]
    );
    Ok(())
}

fn limited(tracks: Vec<DiscoveredTrack>) -> Result<StartedNormalizer, NormalizeError> {
    let input = presentation(tracks);
    let timeline = calibrate(&input).expect("fixture calibrates");
    PassThroughNormalizerFactory.start(&input, &timeline, crate::domain::InputMode::Permissive)
}

fn timing_issue(error: NormalizeError) -> crate::domain::TimestampIssue {
    match error {
        NormalizeError::Timestamp(issue) => *issue,
        other => panic!("expected structured timing error: {other}"),
    }
}

#[test]
fn forward_video_steps_are_bounded_per_track_before_output() -> Result<(), NormalizeError> {
    use crate::domain::{TimestampField, TimestampIssueCode};
    for explicit_dts in [false, true] {
        for step in [499, 500, 501] {
            let mut started = limited(vec![millisecond_video(25, 0)])?;
            let mut out = Vec::new();
            started.normalizer.push(
                packet(0, Some(0), explicit_dts.then_some(0), Some(40)),
                &mut out,
            )?;
            let result = started.normalizer.push(
                packet(0, Some(step), explicit_dts.then_some(step), Some(40)),
                &mut out,
            );
            if step <= 500 {
                result?;
                assert_eq!(
                    out[0].duration(),
                    u64::try_from(step).expect("positive") * 90
                );
                started.normalizer.finish(&mut out)?;
            } else {
                let issue = timing_issue(result.expect_err("jump rejected"));
                assert_eq!(issue.code, TimestampIssueCode::VideoTimestampJump);
                assert_eq!(
                    issue.field,
                    if explicit_dts {
                        TimestampField::Dts
                    } else {
                        TimestampField::Pts
                    }
                );
                assert_eq!((issue.reference, issue.actual), (0, 501));
                assert_eq!(issue.maximum, Some(std::time::Duration::from_millis(500)));
                assert!(
                    out.is_empty(),
                    "do not stretch the previous frame before rejecting"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn video_fallback_duration_and_reordered_pts_have_distinct_policies() -> Result<(), NormalizeError>
{
    use crate::domain::TimestampIssueCode;
    for duration in [500, 501] {
        let mut started = limited(vec![millisecond_video(25, 0)])?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(packet(0, Some(0), Some(0), Some(duration)), &mut out)?;
        let result = started.normalizer.finish(&mut out);
        if duration == 500 {
            result?;
        } else {
            assert_eq!(
                timing_issue(result.expect_err("tail exceeds limit")).code,
                TimestampIssueCode::VideoDurationLimit
            );
            assert!(out.is_empty());
        }
    }
    let mut started = limited(vec![millisecond_video(25, 2)])?;
    let mut out = Vec::new();
    for (pts, dts) in [(0, 0), (1000, 40), (40, 80)] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(dts), Some(40)), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(
        out.len(),
        3,
        "large reordered PTS steps are not decode jumps"
    );
    Ok(())
}

#[test]
fn another_tracks_progress_cannot_hide_a_video_jump() -> Result<(), NormalizeError> {
    let second = TrackBuilder::new(1, MediaKind::Video)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(1000)))
        .build();
    let mut started = limited(vec![millisecond_video(25, 0), second])?;
    let mut out = Vec::new();
    started
        .normalizer
        .push(packet(0, Some(0), Some(0), Some(40)), &mut out)?;
    for t in [0, 400, 800, 1200] {
        started
            .normalizer
            .push(packet(1, Some(t), Some(t), Some(40)), &mut out)?;
    }
    let before = out.len();
    let issue = timing_issue(
        started
            .normalizer
            .push(packet(0, Some(1000), Some(1000), Some(40)), &mut out)
            .expect_err("per-track guard"),
    );
    assert_eq!(issue.track, TrackId(0));
    assert_eq!(out.len(), before);
    Ok(())
}

#[test]
fn audio_gap_classification_keeps_contradictory_dts_out_of_recovery() -> Result<(), NormalizeError>
{
    use crate::domain::TimestampIssueCode as Code;
    for (pts, dts, code) in [
        (2048, Some(2048), Code::AudioGap),
        (2048, None, Code::AudioGap),
        (2048, Some(1024), Code::AudioDtsMismatch),
        (1024, Some(2048), Code::AudioDtsMismatch),
        (1000, Some(1000), Code::TimestampOverlap),
    ] {
        let audio = TrackBuilder::new(0, MediaKind::Audio)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(48000)))
            .build();
        let mut started = limited(vec![audio])?;
        started
            .normalizer
            .configure_audio_recovery(AudioRecoveryPolicy {
                enabled: false,
                ..Default::default()
            })?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(packet(0, Some(0), Some(0), Some(1024)), &mut out)?;
        let issue = timing_issue(
            started
                .normalizer
                .push(packet(0, Some(pts), dts, Some(1024)), &mut out)
                .expect_err("timing issue"),
        );
        assert_eq!(issue.code, code);
        assert_eq!(
            issue.missing_ticks,
            (code == Code::AudioGap).then_some(1024)
        );
        assert_eq!(out.len(), 1);
    }
    Ok(())
}

#[test]
fn initial_audio_mismatch_and_missing_timestamps_are_not_gaps() -> Result<(), NormalizeError> {
    let audio = TrackBuilder::new(0, MediaKind::Audio)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(48000)))
        .build();
    for (pts, dts) in [(Some(100), Some(100)), (Some(0), Some(100))] {
        let mut started = limited(vec![audio.clone()])?;
        let issue = timing_issue(
            started
                .normalizer
                .push(packet(0, pts, dts, Some(1024)), &mut Vec::new())
                .expect_err("initial mismatch"),
        );
        assert_eq!(
            issue.code,
            crate::domain::TimestampIssueCode::InitialTimestampMismatch
        );
        assert_eq!(issue.missing_ticks, None);
    }
    let mut started = limited(vec![audio])?;
    let mut out = Vec::new();
    for _ in 0..3 {
        started
            .normalizer
            .push(packet(0, None, None, None), &mut out)?;
    }
    assert_eq!(
        out.iter().map(NormalizedMedia::pts).collect::<Vec<_>>(),
        [0, 1024, 2048]
    );
    Ok(())
}

#[test]
fn extreme_video_step_is_reported_without_subtraction_overflow() -> Result<(), NormalizeError> {
    let mut started = limited(vec![millisecond_video(25, 0)])?;
    let mut out = Vec::new();
    started.normalizer.push(
        packet(0, Some(i64::MIN), Some(i64::MIN), Some(40)),
        &mut out,
    )?;
    let issue = timing_issue(
        started
            .normalizer
            .push(
                packet(0, Some(i64::MAX), Some(i64::MAX), Some(40)),
                &mut out,
            )
            .expect_err("extreme jump"),
    );
    assert_eq!(
        (issue.reference, issue.actual),
        (i128::from(i64::MIN), i128::from(i64::MAX))
    );
    assert!(out.is_empty());
    Ok(())
}

#[test]
fn variable_opus_duration_uses_the_previous_packet_end_for_gap_detection()
-> Result<(), NormalizeError> {
    let audio = TrackBuilder::new(0, MediaKind::Audio)
        .codec(Codec::Opus)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(48000)))
        .parameters(MediaParameters::Audio {
            sample_rate: nz::u32!(48000),
            channels: nz::u16!(2),
            frame_size: None,
            bit_depth: None,
            timing: AudioTiming::default(),
        })
        .build();
    let mut started = limited(vec![audio])?;
    started
        .normalizer
        .configure_audio_recovery(AudioRecoveryPolicy {
            enabled: false,
            ..Default::default()
        })?;
    let mut out = Vec::new();
    for (pts, duration) in [(0, 960), (960, 480), (1440, 120)] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), Some(duration)), &mut out)?;
    }
    let issue = timing_issue(
        started
            .normalizer
            .push(packet(0, Some(1800), Some(1800), Some(960)), &mut out)
            .expect_err("missing Opus interval"),
    );
    assert_eq!(issue.missing_ticks, Some(240));
    assert_eq!((issue.reference, issue.actual), (1560, 1800));
    assert_eq!(out.len(), 3);
    Ok(())
}

#[test]
fn source_duration_limit_is_not_rounded_to_the_output_clock() -> Result<(), NormalizeError> {
    // These source intervals straddle 500 ms. Policy checks source evidence
    // before rounding it into the 90 kHz packaging clock.
    for (ticks, accepted) in [(15000, true), (15001, false)] {
        let video = TrackBuilder::new(0, MediaKind::Video)
            .timebase(Timebase::new(nz::u32!(1), nz::u32!(30001)))
            .build();
        let input = presentation(vec![video]);
        let timeline = calibrate(&input).expect("fixture calibrates");
        let mut started = PassThroughNormalizerFactory.start(
            &input,
            &timeline,
            crate::domain::InputMode::Permissive,
        )?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(packet(0, Some(0), Some(0), Some(ticks)), &mut out)?;
        let result = started
            .normalizer
            .push(packet(0, Some(ticks), Some(ticks), Some(ticks)), &mut out);
        assert_eq!(result.is_ok(), accepted);
        if accepted {
            started.normalizer.finish(&mut out)?;
            assert_eq!(out.len(), 2);
        } else {
            assert!(out.is_empty());
        }
    }
    Ok(())
}

fn recoverable_audio(id: u32, opus: bool) -> DiscoveredTrack {
    let mut track = TrackBuilder::new(id, MediaKind::Audio)
        .timebase(Timebase::new(nz::u32!(1), nz::u32!(48000)))
        .codec_extradata(&[0x11, 0x90][..])
        .build();
    if opus {
        track.codec = Codec::Opus;
        track.codec_extradata =
            (&b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00"[..]).into();
        if let MediaParameters::Audio { frame_size, .. } = &mut track.parameters {
            *frame_size = None;
        }
    }
    track
}
fn audio_packet(id: u32, pts: i64, ticks: i64, opus: bool) -> Packet {
    let mut packet = packet(id, Some(pts), Some(pts), Some(ticks));
    packet.payload = if opus {
        (&[0xfc, 0xff, 0xfe][..]).into()
    } else {
        (&[0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c][..]).into()
    };
    packet
}

#[test]
fn audio_gaps_preserve_exact_intervals_and_report_episode_transitions() -> Result<(), NormalizeError>
{
    use crate::domain::RecoveryTransition;
    for opus in [false, true] {
        let ticks = if opus { 960 } else { 1024 };
        let mut started = limited(vec![recoverable_audio(0, opus)])?;
        started
            .normalizer
            .configure_audio_recovery(AudioRecoveryPolicy {
                clean_interval: std::time::Duration::from_millis(60),
                ..Default::default()
            })?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(audio_packet(0, 0, ticks, opus), &mut out)?;
        started
            .normalizer
            .push(audio_packet(0, ticks * 4, ticks, opus), &mut out)?;
        assert_eq!(out.len(), 3);
        assert!(
            matches!(&out[1], NormalizedMedia::Gap(gap) if gap.start == ticks && gap.end == ticks * 4)
        );
        for pair in out.windows(2) {
            assert_eq!(
                pair[0].pts().checked_add_unsigned(pair[0].duration()),
                Some(pair[1].pts())
            );
        }
        let notice = started.normalizer.take_notices();
        assert_eq!(notice.len(), 1);
        assert_eq!(notice[0].transition, RecoveryTransition::Degraded);
        assert_eq!(notice[0].status.total_ticks, ticks.unsigned_abs() * 3);
        for i in 5..7 {
            started
                .normalizer
                .push(audio_packet(0, ticks * i, ticks, opus), &mut out)?;
        }
        let recovered = started.normalizer.take_notices();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].transition, RecoveryTransition::Recovered);
        started.normalizer.finish(&mut out)?;
        assert!(started.normalizer.take_notices().is_empty());
    }
    Ok(())
}

#[test]
fn audio_recovery_rejections_emit_neither_repair_nor_offending_packet() -> Result<(), NormalizeError>
{
    use crate::domain::RecoveryRejection as R;
    for (policy, pts, reason) in [
        (
            AudioRecoveryPolicy {
                enabled: false,
                ..Default::default()
            },
            1920,
            R::Disabled,
        ),
        (AudioRecoveryPolicy::default(), 25_920, R::MaximumHole),
        (
            AudioRecoveryPolicy {
                maximum_compensation: std::time::Duration::from_millis(10),
                ..Default::default()
            },
            1920,
            R::MaximumCompensation,
        ),
    ] {
        let mut started = limited(vec![recoverable_audio(0, true)])?;
        started.normalizer.configure_audio_recovery(policy)?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(audio_packet(0, 0, 960, true), &mut out)?;
        let issue = timing_issue(
            started
                .normalizer
                .push(audio_packet(0, pts, 960, true), &mut out)
                .expect_err("repair rejected"),
        );
        assert_eq!(issue.recovery_rejection, Some(reason));
        assert_eq!(out.len(), 1);
        assert!(started.normalizer.take_notices().is_empty());
    }
    Ok(())
}

#[test]
fn audio_recovery_budgets_are_per_track_and_survive_episode_recovery() -> Result<(), NormalizeError>
{
    use crate::domain::RecoveryRejection;
    let mut started = limited(vec![recoverable_audio(0, true), recoverable_audio(1, true)])?;
    started
        .normalizer
        .configure_audio_recovery(AudioRecoveryPolicy {
            maximum_holes: 1,
            clean_interval: std::time::Duration::from_millis(20),
            ..Default::default()
        })?;
    let mut out = Vec::new();
    for id in [0, 1] {
        started
            .normalizer
            .push(audio_packet(id, 0, 960, true), &mut out)?;
        started
            .normalizer
            .push(audio_packet(id, 1920, 960, true), &mut out)?;
    }
    assert_eq!(started.normalizer.take_notices().len(), 4);
    let before = out.len();
    let issue = timing_issue(
        started
            .normalizer
            .push(audio_packet(0, 3840, 960, true), &mut out)
            .expect_err("budget persists"),
    );
    assert_eq!(
        issue.recovery_rejection,
        Some(RecoveryRejection::MaximumHoles)
    );
    assert_eq!(out.len(), before);
    Ok(())
}

#[test]
fn repair_validates_real_packet_before_emitting_and_preserves_prior_notices()
-> Result<(), NormalizeError> {
    let mut started = limited(vec![recoverable_audio(0, true)])?;
    let mut out = Vec::new();
    started
        .normalizer
        .push(audio_packet(0, 0, 960, true), &mut out)?;
    started
        .normalizer
        .push(audio_packet(0, 1920, 960, true), &mut out)?;
    let before = out.len();
    let mut invalid = audio_packet(0, 3840, 960, true);
    invalid.dts = Some(2880);
    assert!(started.normalizer.push(invalid, &mut out).is_err());
    assert_eq!(out.len(), before);
    assert_eq!(started.normalizer.take_notices().len(), 1);
    Ok(())
}

#[test]
fn audio_recovery_accepts_whole_holes_at_limit_and_rounding_boundary() -> Result<(), NormalizeError>
{
    for offset in [-48, 0, 48] {
        let mut started = limited(vec![recoverable_audio(0, true)])?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(audio_packet(0, 0, 960, true), &mut out)?;
        // Below the limit with the full millisecond transport tolerance.
        started
            .normalizer
            .push(audio_packet(0, 24_000 + offset, 960, true), &mut out)?;
        assert_eq!(out.last().map(NormalizedMedia::pts), Some(24_000 + offset));
    }
    let mut started = limited(vec![recoverable_audio(0, true)])?;
    let mut out = Vec::new();
    started
        .normalizer
        .push(audio_packet(0, 0, 960, true), &mut out)?;
    started
        .normalizer
        .push(audio_packet(0, 24_960, 960, true), &mut out)?;
    assert_eq!(out.len(), 3);
    Ok(())
}

#[test]
fn audio_recovery_default_count_and_duration_budgets_are_inclusive() -> Result<(), NormalizeError> {
    use crate::domain::RecoveryRejection;
    for (hole_ticks, allowed, reason) in [
        (960, 10, RecoveryRejection::MaximumHoles),
        (24_000, 2, RecoveryRejection::MaximumCompensation),
    ] {
        let mut started = limited(vec![recoverable_audio(0, true)])?;
        let mut out = Vec::new();
        let mut pts = 0;
        started
            .normalizer
            .push(audio_packet(0, pts, 960, true), &mut out)?;
        for _ in 0..allowed {
            pts += 960 + hole_ticks;
            started
                .normalizer
                .push(audio_packet(0, pts, 960, true), &mut out)?;
        }
        assert_eq!(started.normalizer.take_notices().len(), allowed);
        let before = out.len();
        pts += 960 + hole_ticks;
        let issue = timing_issue(
            started
                .normalizer
                .push(audio_packet(0, pts, 960, true), &mut out)
                .expect_err("next hole exceeds budget"),
        );
        assert_eq!(issue.recovery_rejection, Some(reason));
        assert_eq!(out.len(), before);
    }
    Ok(())
}

#[test]
fn recovery_preserves_fractional_cadence_and_variable_opus_frames() -> Result<(), NormalizeError> {
    let mut audio = recoverable_audio(0, false);
    audio.timebase = Timebase::new(nz::u32!(1), nz::u32!(1000));
    let mut started = limited(vec![audio])?;
    let mut out = Vec::new();
    for pts in [0, 64, 85, 107] {
        started
            .normalizer
            .push(audio_packet(0, pts, 21, false), &mut out)?;
    }
    assert_eq!(
        out.iter().map(NormalizedMedia::pts).collect::<Vec<_>>(),
        [0, 1024, 3072, 4096, 5120]
    );
    let mut started = limited(vec![recoverable_audio(0, true)])?;
    let mut out = Vec::new();
    for (pts, duration, toc) in [(0, 480, 0xf4), (1440, 240, 0xec), (1920, 960, 0xfc)] {
        let mut packet = audio_packet(0, pts, duration, true);
        packet.payload = vec![toc, 0xff, 0xfe].into();
        started.normalizer.push(packet, &mut out)?;
    }
    assert_eq!(
        out.iter()
            .map(NormalizedMedia::duration)
            .collect::<Vec<_>>(),
        [480, 960, 240, 240, 960]
    );
    assert_eq!(started.normalizer.take_notices().len(), 2);
    Ok(())
}

#[test]
fn non_integral_audio_holes_need_no_codec_recovery_configuration() -> Result<(), NormalizeError> {
    let mut audio = recoverable_audio(0, false);
    audio.codec_extradata = (&[0x11, 0x90, 0x01][..]).into();
    let mut started = limited(vec![audio])?;
    let mut out = Vec::new();
    for pts in [0, 1024, 2200] {
        started
            .normalizer
            .push(audio_packet(0, pts, 1024, false), &mut out)?;
    }
    assert!(matches!(&out[2], NormalizedMedia::Gap(gap) if gap.start == 2048 && gap.end == 2200));
    assert_eq!(out[3].pts(), 2200);
    let notices = started.normalizer.take_notices();
    assert_eq!(notices[0].status.method, crate::domain::RecoveryMethod::Gap);
    assert_eq!(notices[0].status.replacement_ticks, 0);
    assert_eq!(notices[0].status.missing_ticks, 152);
    Ok(())
}

#[test]
fn he_aac_missing_intervals_do_not_construct_sbr_or_ps_packets()
-> Result<(), Box<dyn std::error::Error>> {
    // Real AudioToolbox ASCs: mono/stereo HE and stereo HEv2, at both rates.
    // These declarations do not contain the packet-level SBR/PS state.
    for asc in [
        &[0x13, 0x88, 0x56, 0xe5, 0xa0][..],
        &[0x13, 0x90, 0x56, 0xe5, 0xa0],
        &[0x13, 0x88, 0x56, 0xe5, 0xa5, 0x48, 0x80],
        &[0x13, 0x08, 0x56, 0xe5, 0x98],
        &[0x13, 0x10, 0x56, 0xe5, 0x98],
        &[0x13, 0x08, 0x56, 0xe5, 0x9d, 0x48, 0x80],
    ] {
        let mut track = recoverable_audio(0, false);
        track.parameters =
            crate::domain::aac::parameters(asc).map_err(|error| error.to_string())?;
        track.codec_extradata = asc.into();
        let MediaParameters::Audio { sample_rate, .. } = track.parameters else {
            panic!("AAC parameters");
        };
        track.timebase = Timebase::new(nz::u32!(1), sample_rate);
        let mut started = limited(vec![track])?;
        let mut out = Vec::new();
        for pts in [0, 2048] {
            started
                .normalizer
                .push(audio_packet(0, pts, 2048, false), &mut out)?;
        }
        started
            .normalizer
            .push(audio_packet(0, 6144, 2048, false), &mut out)?;
        assert!(
            matches!(&out[2], NormalizedMedia::Gap(gap) if gap.start == 4096 && gap.end == 6144)
        );
        assert_eq!(out.len(), 4);
        assert_eq!(started.normalizer.take_notices().len(), 1);
    }
    Ok(())
}

fn fixed_video(rate: FrameRate, delay: u32) -> DiscoveredTrack {
    let mut track = millisecond_video(30, delay);
    track.video_cadence = crate::domain::VideoCadence::Fixed {
        rate,
        source: crate::domain::CadenceSource::H264Vui,
        scope: crate::domain::CadenceScope::ProgressiveFrames,
    };
    track
}
fn mode_start(
    track: DiscoveredTrack,
    mode: crate::domain::InputMode,
) -> Result<StartedNormalizer, NormalizeError> {
    let input = presentation(vec![track]);
    let timeline =
        calibrate(&input).map_err(|e| NormalizeError::InvalidPlan(e.to_string().into()))?;
    PassThroughNormalizerFactory.start(&input, &timeline, mode)
}
#[test]
fn declared_cadence_modes_distinguish_late_early_and_nominal() -> Result<(), NormalizeError> {
    use crate::domain::{InputMode as M, RecoveryTransition as T, TimestampIssueCode as C};
    for mode in [M::Strict, M::Permissive] {
        for (actual, accepted) in [
            (41, true),
            (43, mode == M::Permissive),
            (80, mode == M::Permissive),
            (73, mode == M::Permissive),
            (37, false),
        ] {
            let mut started = mode_start(
                fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0),
                mode,
            )?;
            let mut out = Vec::new();
            started
                .normalizer
                .push(packet(0, Some(0), Some(0), None), &mut out)?;
            let result = started
                .normalizer
                .push(packet(0, Some(actual), Some(actual), None), &mut out);
            assert_eq!(result.is_ok(), accepted, "{mode:?} at {actual}: {result:?}");
            if let Err(error) = result {
                assert_eq!(timing_issue(error).code, C::VideoCadenceViolation);
                assert!(out.is_empty());
            } else {
                started.normalizer.finish(&mut out)?;
                assert_eq!(out.len(), if actual > 41 { 3 } else { 2 });
            }
            let notices = started.normalizer.take_notices();
            if accepted && actual > 41 {
                assert_eq!(notices.len(), 1);
                assert_eq!(notices[0].transition, T::Degraded);
            } else {
                assert!(notices.is_empty());
            }
        }
        let mut nominal = mode_start(millisecond_video(25, 0), mode)?;
        let mut out = Vec::new();
        for pts in [0, 70, 90, 400] {
            nominal
                .normalizer
                .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
        }
        nominal.normalizer.finish(&mut out)?;
        assert!(nominal.normalizer.take_notices().is_empty());
        assert_eq!(nominal.normalizer.take_video_intervals().len(), 3);
    }
    Ok(())
}
#[test]
fn fractional_grid_is_anchored_and_lateness_reanchors_only_in_permissive_mode()
-> Result<(), NormalizeError> {
    use crate::domain::InputMode;
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(30000), nz::u32!(1001)), 0),
        InputMode::Strict,
    )?;
    let mut out = Vec::new();
    for index in 0..3000 {
        let pts = (index * 1001 + 15) / 30;
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 3000);
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0),
        InputMode::Permissive,
    )?;
    out.clear();
    for pts in [0, 73, 113, 153] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(started.normalizer.take_notices().len(), 1);
    assert_eq!(out.len(), 5);
    Ok(())
}
#[test]
fn reordered_holes_are_validated_before_decode_order_release_including_flush()
-> Result<(), NormalizeError> {
    use crate::domain::InputMode;
    for (flush, mode) in [
        (false, InputMode::Strict),
        (true, InputMode::Strict),
        (false, InputMode::Permissive),
        (true, InputMode::Permissive),
    ] {
        let mut started = mode_start(
            fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 2),
            mode,
        )?;
        let mut out = Vec::new();
        for (pts, dts) in [(0, -80), (120, -40), (40, 0)] {
            started
                .normalizer
                .push(packet(0, Some(pts), Some(dts), None), &mut out)?;
        }
        assert!(out.is_empty());
        let result = if flush {
            started.normalizer.finish(&mut out)
        } else {
            started
                .normalizer
                .push(packet(0, Some(160), Some(40), None), &mut out)?;
            started
                .normalizer
                .push(packet(0, Some(200), Some(80), None), &mut out)
        };
        let issue = timing_issue(result.expect_err("80ms picture absent"));
        assert_eq!(
            issue.code,
            crate::domain::TimestampIssueCode::VideoCadenceViolation
        );
        assert_eq!(
            issue.recovery_rejection,
            (mode == InputMode::Permissive)
                .then_some(crate::domain::RecoveryRejection::UnsupportedConfiguration)
        );
        assert!(started.normalizer.take_notices().is_empty());
        assert!(out.iter().all(|s| s.pts() < 10800));
    }
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 2),
        InputMode::Strict,
    )?;
    let mut out = Vec::new();
    for (pts, dts) in [(0, -80), (120, -40), (40, 0), (80, 40), (160, 80)] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(dts), Some(40)), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 5);
    Ok(())
}
#[test]
fn unavailable_validation_never_recovers_and_conflicts_fail_both_modes()
-> Result<(), NormalizeError> {
    use crate::domain::{
        CadenceSource as S, CadenceUnavailable as U, InputMode as M, RecoveryTransition,
        VideoCadence as C,
    };
    for mode in [M::Strict, M::Permissive] {
        let mut track = millisecond_video(25, 0);
        track.video_cadence = C::Conflicting { source: S::H264Vui };
        assert!(mode_start(track, mode).is_err());
        let mut track = millisecond_video(25, 0);
        track.video_cadence = C::Unverifiable {
            rate: None,
            source: S::H264Vui,
            reason: U::PictureStructure,
        };
        let result = mode_start(track, mode);
        if mode == M::Strict {
            assert!(result.is_err());
            continue;
        }
        let mut started = result?;
        let mut out = Vec::new();
        for i in 0..1000 {
            started
                .normalizer
                .push(packet(0, Some(i * 40), Some(i * 40), Some(40)), &mut out)?;
        }
        started.normalizer.finish(&mut out)?;
        let notices = started.normalizer.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].transition, RecoveryTransition::Unavailable);
    }
    Ok(())
}
#[test]
fn video_compensation_budgets_and_hysteresis_use_media_time() -> Result<(), NormalizeError> {
    use crate::domain::{InputMode, RecoveryTransition as T};
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0),
        InputMode::Permissive,
    )?;
    let mut out = Vec::new();
    for pts in [0, 540, 1080] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    let notices = started.normalizer.take_notices();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[1].status.total_ticks, 1000);
    assert!(
        started
            .normalizer
            .push(packet(0, Some(1123), Some(1123), None), &mut out)
            .is_err()
    );
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0),
        InputMode::Permissive,
    )?;
    out.clear();
    for pts in [0, 80] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    for i in 1..=750 {
        let pts = 80 + i * 40;
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    let notices = started.normalizer.take_notices();
    assert_eq!(
        notices.iter().map(|n| n.transition).collect::<Vec<_>>(),
        vec![T::Degraded, T::Recovered]
    );
    assert_eq!(notices[1].status.total_ticks, 40);
    Ok(())
}

#[test]
fn strict_audio_rejects_gaps_for_both_supported_codecs() -> Result<(), NormalizeError> {
    for opus in [false, true] {
        let ticks = if opus { 960 } else { 1024 };
        let mut started = mode_start(recoverable_audio(0, opus), crate::domain::InputMode::Strict)?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(audio_packet(0, 0, ticks, opus), &mut out)?;
        let error = started
            .normalizer
            .push(audio_packet(0, ticks * 2, ticks, opus), &mut out)
            .expect_err("strict rejects real holes");
        let issue = timing_issue(error);
        assert_eq!(issue.code, crate::domain::TimestampIssueCode::AudioGap);
        assert_eq!(
            issue.recovery_rejection,
            Some(crate::domain::RecoveryRejection::Disabled)
        );
        assert_eq!(out.len(), 1);
        assert!(started.normalizer.take_notices().is_empty());
    }
    Ok(())
}
#[test]
fn low_declared_rates_and_fractional_excess_keep_exact_clocks() -> Result<(), NormalizeError> {
    use crate::domain::InputMode;
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(1), nz::u32!(1)), 0),
        InputMode::Strict,
    )?;
    let mut out = Vec::new();
    for pts in [0, 1000, 2000] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 3);
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(30000), nz::u32!(1001)), 0),
        InputMode::Permissive,
    )?;
    out.clear();
    for pts in [0, 50] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    let notices = started.normalizer.take_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].status.timebase,
        Timebase::new(nz::u32!(1), nz::u32!(30000))
    );
    assert_eq!(notices[0].status.total_ticks, 499);
    Ok(())
}
#[test]
fn video_count_budget_expires_at_window_boundary_and_new_sessions_reset()
-> Result<(), NormalizeError> {
    use crate::domain::InputMode;
    let track = fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0);
    for expire in [false, true] {
        let mut started = mode_start(track.clone(), InputMode::Permissive)?;
        let mut out = Vec::new();
        let mut pts = 0;
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
        for _ in 0..10 {
            pts += 44;
            started
                .normalizer
                .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
        }
        if expire {
            while pts < 60_440 {
                pts += 40;
                started
                    .normalizer
                    .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
            }
        }
        pts += 44;
        let result = started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out);
        assert_eq!(result.is_ok(), expire);
        let mut fresh = mode_start(track.clone(), InputMode::Permissive)?;
        out.clear();
        for pts in [0, 44] {
            fresh
                .normalizer
                .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
        }
        assert_eq!(fresh.normalizer.take_notices()[0].status.total_holes, 1);
    }
    Ok(())
}
#[test]
fn gradual_drift_and_extreme_video_timestamps_never_become_silent_calibration()
-> Result<(), NormalizeError> {
    let track = fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0);
    let mut started = mode_start(track.clone(), crate::domain::InputMode::Strict)?;
    let mut out = Vec::new();
    for pts in [0, 41] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    assert!(
        started
            .normalizer
            .push(packet(0, Some(82), Some(82), None), &mut out)
            .is_err()
    );
    let mut started = mode_start(track, crate::domain::InputMode::Permissive)?;
    out.clear();
    started
        .normalizer
        .push(packet(0, Some(i64::MIN), Some(i64::MIN), None), &mut out)?;
    assert!(
        started
            .normalizer
            .push(packet(0, Some(i64::MAX), Some(i64::MAX), None), &mut out)
            .is_err()
    );
    assert!(started.normalizer.take_notices().is_empty());
    Ok(())
}
#[test]
fn av1_hidden_frames_inside_a_temporal_unit_do_not_add_presentation_intervals()
-> Result<(), NormalizeError> {
    let mut track = fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0);
    track.codec = Codec::Av1;
    track.video_cadence = crate::domain::VideoCadence::Fixed {
        rate: FrameRate::new(nz::u32!(25), nz::u32!(1)),
        source: crate::domain::CadenceSource::Av1Sequence,
        scope: crate::domain::CadenceScope::SingleLayerTemporalUnits,
    };
    let mut started = mode_start(track, crate::domain::InputMode::Strict)?;
    let mut out = Vec::new();
    for pts in [0, 40, 80] {
        let mut p = packet(0, Some(pts), Some(pts), None);
        p.payload = (&[0x32, 1, 0x00, 0x32, 1, 0x10][..]).into();
        started.normalizer.push(p, &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 3);
    assert!(started.normalizer.take_notices().is_empty());
    Ok(())
}

#[test]
fn small_presentation_reorder_depth_can_retain_a_longer_decode_order_run()
-> Result<(), NormalizeError> {
    let track = fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 1);
    let mut started = mode_start(track, crate::domain::InputMode::Strict)?;
    let mut out = Vec::new();
    let pictures = [0, 400, 40, 80, 120, 160, 200, 240, 280, 320, 360, 440];
    for (index, pts) in pictures.into_iter().enumerate() {
        let dts = i64::try_from(index).expect("small index") * 40 - 40;
        started
            .normalizer
            .push(packet(0, Some(pts), Some(dts), Some(40)), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), pictures.len());
    assert_eq!(
        out.iter().map(NormalizedMedia::pts).collect::<Vec<_>>(),
        pictures.map(|pts| pts * 90)
    );
    Ok(())
}

fn video_gaps(mode: crate::domain::InputMode) -> Result<StartedNormalizer, NormalizeError> {
    mode_start(
        fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0),
        mode,
    )
}

#[test]
fn video_gaps_preserve_samples_and_resume_original_clock() -> Result<(), NormalizeError> {
    use crate::domain::{InputMode, RecoveryMethod, RecoveryTransition};
    for explicit_dts in [false, true] {
        for end in [43, 73, 80, 540] {
            let mut started = video_gaps(InputMode::Permissive)?;
            let mut out = Vec::new();
            for pts in [0, end, end + 40] {
                started.normalizer.push(
                    packet(0, Some(pts), explicit_dts.then_some(pts), None),
                    &mut out,
                )?;
            }
            started.normalizer.finish(&mut out)?;
            assert_eq!(out.len(), 4);
            assert!(
                matches!(&out[0], NormalizedMedia::Video(v) if v.pts == 0 && v.duration == 3_600)
            );
            assert!(
                matches!(&out[1], NormalizedMedia::Gap(g) if g.start == 3_600 && g.end == end * 90)
            );
            for (item, pts) in out[2..].iter().zip([end, end + 40]) {
                assert!(
                    matches!(item, NormalizedMedia::Video(v) if v.pts == pts * 90 && v.dts == pts * 90 && v.duration == 3_600)
                );
            }
            let notices = started.normalizer.take_notices();
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].transition, RecoveryTransition::Degraded);
            assert_eq!(notices[0].status.method, RecoveryMethod::Gap);
            assert_eq!(notices[0].status.missing_ticks, end.abs_diff(40));
            assert_eq!(notices[0].status.replacement_ticks, 0);
        }
    }
    Ok(())
}

#[test]
fn video_gap_rejection_is_atomic_and_has_no_compensation_notice() -> Result<(), NormalizeError> {
    use crate::domain::InputMode;
    for (pts, dts, duration) in [
        (80, Some(40), None),
        (80, Some(81), None),
        (80, Some(80), Some(-1)),
        (541, Some(541), None),
    ] {
        let mut started = video_gaps(InputMode::Permissive)?;
        let mut out = Vec::new();
        started
            .normalizer
            .push(packet(0, Some(0), Some(0), None), &mut out)?;
        assert!(
            started
                .normalizer
                .push(packet(0, Some(pts), dts, duration), &mut out)
                .is_err()
        );
        assert!(out.is_empty());
        assert!(started.normalizer.take_notices().is_empty());
        // A failure flush may preserve the preceding valid picture only.
        let _ = started.normalizer.finish(&mut out);
        assert!(
            out.iter()
                .all(|m| matches!(m, NormalizedMedia::Video(v) if v.pts == 0))
        );
    }
    let mut strict = video_gaps(InputMode::Strict)?;
    let mut out = Vec::new();
    strict
        .normalizer
        .push(packet(0, Some(0), None, None), &mut out)?;
    assert!(
        strict
            .normalizer
            .push(packet(0, Some(80), None, None), &mut out)
            .is_err()
    );
    assert!(out.is_empty());
    assert!(strict.normalizer.take_notices().is_empty());
    Ok(())
}

#[test]
fn video_gap_rounding_and_nominal_timing_do_not_infer_absence() -> Result<(), NormalizeError> {
    use crate::domain::InputMode;
    let mut started = video_gaps(InputMode::Permissive)?;
    let mut out = Vec::new();
    for pts in [0, 41, 80, 120] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 4);
    assert!(out.iter().all(|m| matches!(m, NormalizedMedia::Video(_))));
    assert!(started.normalizer.take_notices().is_empty());
    let mut nominal = mode_start(millisecond_video(25, 0), InputMode::Permissive)?;
    let mut out = Vec::new();
    for pts in [0, 100, 400] {
        nominal
            .normalizer
            .push(packet(0, Some(pts), None, None), &mut out)?;
    }
    nominal.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 3);
    assert!(nominal.normalizer.take_notices().is_empty());
    Ok(())
}

#[test]
fn video_gap_fractional_grid_does_not_rebase_following_packets() -> Result<(), NormalizeError> {
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(30_000), nz::u32!(1_001)), 0),
        crate::domain::InputMode::Permissive,
    )?;
    let mut out = Vec::new();
    // Millisecond ingress rounds the rational cadence; skip one picture.
    for pts in [0, 33, 100, 133, 167, 200] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(pts), None), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert!(matches!(&out[2], NormalizedMedia::Gap(g) if g.start == 6_006 && g.end == 9_000));
    let pts: Vec<_> = out
        .iter()
        .filter_map(|m| match m {
            NormalizedMedia::Video(v) => Some(v.pts),
            _ => None,
        })
        .collect();
    assert_eq!(pts, [0, 2_970, 9_000, 11_970, 15_030, 18_000]);
    let notice = started.normalizer.take_notices().remove(0);
    assert_eq!(notice.status.replacement_ticks, 0);
    Ok(())
}

#[test]
fn video_gap_budget_rejects_without_publishing_a_partial_interval() -> Result<(), NormalizeError> {
    use crate::domain::{InputMode, RecoveryRejection};
    let mut started = video_gaps(InputMode::Permissive)?;
    let mut out = Vec::new();
    for pts in [0, 540, 1080] {
        started
            .normalizer
            .push(packet(0, Some(pts), None, None), &mut out)?;
    }
    let before = out.clone();
    let issue = timing_issue(
        started
            .normalizer
            .push(packet(0, Some(1123), None, None), &mut out)
            .expect_err("one-second rolling budget is full"),
    );
    assert_eq!(
        issue.recovery_rejection,
        Some(RecoveryRejection::MaximumCompensation)
    );
    assert_eq!(out, before);
    let notices = started.normalizer.take_notices();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[1].status.total_ticks, 1000);
    assert_eq!(notices[1].status.total_holes, 2);
    Ok(())
}

#[test]
fn video_gap_episode_recovers_on_real_media_without_resetting_budget() -> Result<(), NormalizeError>
{
    use crate::domain::{InputMode, RecoveryRejection, RecoveryTransition};
    let mut started = video_gaps(InputMode::Permissive)?;
    let mut out = Vec::new();
    started
        .normalizer
        .push(packet(0, Some(0), None, None), &mut out)?;
    started
        .normalizer
        .push(packet(0, Some(540), None, None), &mut out)?;
    for pts in (580..=30_540).step_by(40) {
        out.clear();
        started
            .normalizer
            .push(packet(0, Some(pts), None, None), &mut out)?;
    }
    let notices = started.normalizer.take_notices();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[0].transition, RecoveryTransition::Degraded);
    assert_eq!(notices[1].transition, RecoveryTransition::Recovered);
    assert_eq!(notices[1].status.total_ticks, 500);
    started
        .normalizer
        .push(packet(0, Some(31_080), None, None), &mut out)?;
    let notice = started.normalizer.take_notices().remove(0);
    assert_eq!(notice.transition, RecoveryTransition::Degraded);
    assert_eq!(notice.status.episode_holes, 1);
    assert_eq!(notice.status.total_holes, 2);
    let error = started
        .normalizer
        .push(packet(0, Some(31_123), None, None), &mut out)
        .expect_err("recovery did not reset rolling budget");
    assert_eq!(
        timing_issue(error).recovery_rejection,
        Some(RecoveryRejection::MaximumCompensation)
    );
    Ok(())
}

#[test]
fn video_gap_extreme_clock_fails_without_partial_output() -> Result<(), NormalizeError> {
    let mut track = fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 0);
    track.timebase = Timebase::hz90k();
    let mut started = mode_start(track, crate::domain::InputMode::Permissive)?;
    let mut out = Vec::new();
    started.normalizer.push(
        packet(0, Some(i64::MAX - 7200), Some(i64::MAX - 7200), None),
        &mut out,
    )?;
    assert!(
        started
            .normalizer
            .push(packet(0, Some(i64::MAX), Some(i64::MAX), None), &mut out)
            .is_err()
    );
    assert!(out.is_empty());
    assert!(started.normalizer.take_notices().is_empty());
    Ok(())
}

#[test]
fn video_gap_activation_does_not_infer_holes_from_reordered_pts() -> Result<(), NormalizeError> {
    let mut started = mode_start(
        fixed_video(FrameRate::new(nz::u32!(25), nz::u32!(1)), 2),
        crate::domain::InputMode::Permissive,
    )?;
    let mut out = Vec::new();
    for (pts, dts) in [(0, -80), (120, -40), (40, 0), (80, 40)] {
        started
            .normalizer
            .push(packet(0, Some(pts), Some(dts), None), &mut out)?;
    }
    started.normalizer.finish(&mut out)?;
    assert_eq!(out.len(), 4);
    assert!(out.iter().all(|m| matches!(m, NormalizedMedia::Video(_))));
    assert!(started.normalizer.take_notices().is_empty());
    Ok(())
}
