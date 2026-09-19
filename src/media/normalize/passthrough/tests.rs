use crate::{
    domain::{
        AudioTiming, AudioTrim, Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters,
        Payload, TickDuration, Timebase, TrackId, fixtures::TrackBuilder,
    },
    media::{
        NormalizeError, NormalizedSample, NormalizerFactory, PresentationPlan, StartedNormalizer,
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
        .start(&input, &timeline)
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
            NormalizedSample::Audio(sample) => {
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
    let mut output = Vec::new();
    started
        .normalizer
        .push(packet(0, Some(0), Some(0), Some(21)), &mut output)
        .expect("first packet anchors the clock");

    let error = started
        .normalizer
        .push(packet(0, Some(100), Some(100), Some(21)), &mut output)
        .expect_err("a 100 ms jump is not timestamp quantization");
    assert!(error.to_string().contains("discontinuity"));
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
        NormalizedSample::Audio(sample)
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
        NormalizedSample::Audio(sample) if sample.trim.trailing_samples == 280
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
    assert!(error.to_string().contains("video DTS must increase"));
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
            NormalizedSample::Video(sample) => (sample.pts, sample.dts, sample.duration),
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

fn video_timing(output: &[NormalizedSample]) -> Vec<(i64, i64, TickDuration)> {
    output
        .iter()
        .map(|sample| match sample {
            NormalizedSample::Video(sample) => (sample.pts, sample.dts, sample.duration),
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
            NormalizedSample::Video(sample) => (sample.pts, sample.dts, sample.duration),
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
            NormalizedSample::Video(sample) => sample,
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
        [NormalizedSample::Subtitle(SubtitleSample {
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
        [NormalizedSample::Subtitle(SubtitleSample {
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

        let [NormalizedSample::Subtitle(cue)] = output.as_slice() else {
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
                assert!(
                    error
                        .to_string()
                        .contains(&format!("audio {field} discontinuity"))
                );
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
                assert!(error.to_string().contains("video DTS must increase"));
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
            assert!(error.to_string().contains("video PTS must increase"));
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
