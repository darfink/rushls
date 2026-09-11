mod fixtures;
use super::*;
use crate::{
    delivery::hls::fixtures as hls,
    mux::{InitializationSegment, PackagedSegment},
};
use fixtures::*;
type Result = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn patterns_expand_utc_and_reject_unsafe_or_unknown_names() -> Result {
    let pattern =
        Pattern::parse("{stream}/{publication}/{time:%Y/%m/%d-%H%M%S}/{rendition}_{segment}.mp4")?;
    assert_eq!(
        pattern.expand(
            "org/camera",
            "uuid",
            "audio",
            42,
            SystemTime::UNIX_EPOCH,
            false
        )?,
        PathBuf::from("org/camera/uuid/1970/01/01-000000/audio_42.mp4")
    );
    assert_eq!(
        pattern
            .expand(
                "org/camera",
                "uuid",
                "captions",
                42,
                SystemTime::UNIX_EPOCH,
                true
            )?
            .extension()
            .and_then(|s| s.to_str()),
        Some("vtt")
    );
    for bad in [
        "",
        "/absolute",
        "../escape",
        "a//b",
        "a/./b",
        "a/../b",
        "a\\b",
        "a\0b",
    ] {
        assert!(
            pattern
                .expand(bad, "id", "audio", 0, SystemTime::UNIX_EPOCH, false)
                .is_err(),
            "{bad:?}"
        );
    }
    for bad in [
        "",
        "{unknown}",
        "{time:%Q}",
        "{time:}",
        "{stream",
        "x}",
        "../{stream}",
        "/{stream}",
    ] {
        assert!(Pattern::parse(bad).is_err(), "{bad:?}");
    }
    Ok(())
}

/// A single expand of an `…{segment}` pattern succeeds, so the collapse is only
/// visible by comparing two expansions that differ in nothing but the segment.
#[test]
fn a_segment_erased_by_the_subtitle_suffix_is_refused_at_parse() -> Result {
    for bad in [
        "{rendition}.{segment}",
        "{stream}/{publication}/{rendition}.{segment}",
        "{stream}/{rendition}.{segment}x",
    ] {
        assert!(Pattern::parse(bad).is_err(), "{bad:?}");
    }
    // Keeping `{segment}` before the final dot leaves the suffix something to
    // replace and the segment something to distinguish.
    for good in [
        "{rendition}_{segment}",
        "{rendition}_{segment}.mp4",
        "{rendition}.{segment}.mp4",
        "{rendition}.{segment}.{segment}.mp4",
        "{segment}",
        // No `{segment}` at all is a documented choice; the writer's no-overwrite
        // rule is what handles the collisions it may cause.
        "same.mp4",
        "{stream}/{publication}/{time:%Y%m%d}/{rendition}.mp4",
    ] {
        assert!(Pattern::parse(good).is_ok(), "{good:?}");
    }
    Ok(())
}

#[tokio::test]
async fn completed_segments_survive_reconnect_with_their_exact_initialization() -> Result {
    let temp = Temp::new();
    let recorder = Recorder::start(&temp.config(), Events::default(), ProcessMeters::default())?;
    let factory = factory(recorder.clone());
    for version in [1, 2] {
        let mut publisher = factory.start(&StreamId::new("org/camera"), presentation())?;
        publisher.write(hls::initialization(0, version))?;
        publisher.write(hls::chunk(0, 0, 0, 0))?;
        assert_eq!(
            temp.files()?.len(),
            usize::from(version - 1),
            "open segments do not create files"
        );
        publisher.write(hls::chunk(0, 0, 1, 1))?;
        publisher.write(complete(0, 0, 2))?;
        publisher.finish(FinishReason::Final)?;
        drop(publisher);
        recorder.drain(Duration::from_secs(5)).await;
    }
    let files = temp.files()?;
    assert_eq!(files.len(), 2);
    for (file, version) in files.iter().zip([1, 2]) {
        let body = std::fs::read(file)?;
        assert_eq!(body[0], version);
        assert_eq!(&body[1..=hls::PART_BYTES], vec![0; hls::PART_BYTES]);
        assert_eq!(&body[1 + hls::PART_BYTES..], vec![1; hls::PART_BYTES]);
    }
    assert_eq!(recorder.shared.bytes.load(Ordering::Acquire), 0);
    Ok(())
}

#[tokio::test]
async fn exhausted_budget_drops_the_whole_segment_then_recovers() -> Result {
    let temp = Temp::new();
    let mut config = temp.config();
    config.maximum_pending_bytes = 1500;
    let log = Arc::new(Log::default());
    let meters = ProcessMeters::default();
    let recorder = Recorder::start(&config, Events::new(log.clone()), meters.clone())?;
    let mut publisher =
        factory(recorder.clone()).start(&StreamId::new("camera"), presentation())?;
    publisher.write(hls::initialization(0, 1))?;
    for index in 0..3 {
        publisher.write(hls::chunk(0, 0, index, i64::from(index)))?;
    }
    publisher.write(complete(0, 0, 3))?;
    recorder.drain(Duration::from_secs(5)).await;
    assert!(temp.files()?.is_empty());
    publisher.write(hls::chunk(0, 1, 0, 3))?;
    publisher.write(complete(1, 3, 1))?;
    recorder.drain(Duration::from_secs(5)).await;
    assert_eq!(temp.files()?.len(), 1);
    // One edge event, then a recovery that accounts for the dropped segment.
    match log.0.lock().as_slice() {
        [
            NodeEvent::RecordingFailed { .. },
            NodeEvent::RecordingRecovered { lost: 1 },
        ] => {}
        other => panic!("expected a failure edge then a recovery of one segment: {other:?}"),
    }
    assert_eq!(meters.snapshot().recording_segments_lost, 1);
    assert_eq!(recorder.shared.bytes.load(Ordering::Acquire), 0);
    Ok(())
}

#[tokio::test]
async fn superseded_publishers_do_not_export_stale_media() -> Result {
    let temp = Temp::new();
    let recorder = Recorder::start(&temp.config(), Events::default(), ProcessMeters::default())?;
    let factory = factory(recorder.clone());
    let stream = StreamId::new("camera");
    let mut old = factory.start(&stream, presentation())?;
    old.write(hls::initialization(0, 1))?;
    old.write(hls::chunk(0, 0, 0, 0))?;
    let _new = factory.start(&stream, presentation())?;
    assert_eq!(old.write(complete(0, 0, 1))?, PublishOutcome::Superseded);
    drop(old);
    recorder.drain(Duration::from_secs(5)).await;
    assert!(temp.files()?.is_empty());
    assert_eq!(recorder.shared.bytes.load(Ordering::Acquire), 0);
    Ok(())
}

#[test]
fn atomic_commit_refuses_overwrite_and_symlink_escape() -> Result {
    let temp = Temp::new();
    let root = filesystem::root(&temp.0)?;
    let body = [Payload::from(b"init-media".as_slice())];
    filesystem::write(&root, std::path::Path::new("nested/segment.mp4"), &body)?;
    assert!(
        filesystem::write(
            &root,
            std::path::Path::new("nested/segment.mp4"),
            &[Payload::from(b"replacement".as_slice())]
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(temp.0.join("nested/segment.mp4"))?,
        b"init-media"
    );
    let outside = Temp::new();
    std::fs::create_dir_all(&outside.0)?;
    std::os::unix::fs::symlink(&outside.0, temp.0.join("escape"))?;
    assert!(filesystem::write(&root, std::path::Path::new("escape/stolen.mp4"), &body).is_err());
    assert!(outside.files()?.is_empty());
    assert_eq!(
        std::fs::read_dir(temp.0.join("nested"))?.count(),
        1,
        "temporary file cleaned after collision"
    );
    Ok(())
}

#[tokio::test]
async fn standalone_webvtt_gets_exactly_one_header() -> Result {
    let temp = Temp::new();
    let recorder = Recorder::start(&temp.config(), Events::default(), ProcessMeters::default())?;
    let factory = factory(recorder.clone());
    let presentation = hls::presentation(vec![hls::subtitle(0), hls::audio(1)]);
    let mut publisher = factory.start(&StreamId::new("camera"), presentation)?;
    publisher.write(PackagedMedia::Initialization(InitializationSegment {
        rendition_id: PackagingRenditionId(0),
        version: 1,
        payload: Payload::from(b"WEBVTT\n\n".as_slice()),
    }))?;
    publisher.write(PackagedMedia::Segment(PackagedSegment {
        rendition_id: PackagingRenditionId(0),
        packaging_segment_id: PackagingSegmentId(0),
        media_start: 0,
        duration: 6,
        independent: true,
        payload: Payload::from(b"WEBVTT-cue-id\n00:00.000 --> 00:01.000\nhello\n\n".as_slice()),
    }))?;
    recorder.drain(Duration::from_secs(5)).await;
    let files = temp.files()?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].extension().and_then(|x| x.to_str()), Some("vtt"));
    assert_eq!(
        std::fs::read_to_string(&files[0])?,
        "WEBVTT\n\nWEBVTT-cue-id\n00:00.000 --> 00:01.000\nhello\n\n"
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end proof covers video and audio codecs"
)]
async fn real_cmaf_recordings_demux_individually_with_all_samples() -> Result {
    use crate::{
        domain::{
            AudioTiming, AudioTrim, Codec, FrameRate, MediaKind, MediaParameters, Timebase,
            TrackId, fixtures::TrackBuilder,
        },
        media::{AudioSample, NormalizedSample, VideoSample},
        mux::{
            MuxerFactory, MuxerStartRequest, PassThroughMuxerFactory,
            fixtures::{
                AAC_EXTRADATA, AAC_FRAME, H264_EXTRADATA, H264_IDR, H264_P, discarded_events,
            },
        },
        segment::{SegmentationPlan, fixtures::PlanBuilder},
    };
    for video in [false, true] {
        let temp = Temp::new();
        let recorder =
            Recorder::start(&temp.config(), Events::default(), ProcessMeters::default())?;
        let timebase = Timebase::new(
            nz::u32!(1),
            if video { nz::u32!(2) } else { nz::u32!(48_000) },
        );
        let track = TrackBuilder::new(
            0,
            if video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
        )
        .timebase(timebase)
        .parameters(if video {
            MediaParameters::Video {
                width: nz::u32!(16),
                height: nz::u32!(16),
                frame_rate: Some(FrameRate::new(nz::u32!(2), nz::u32!(1))),
                video_delay: 0,
            }
        } else {
            MediaParameters::Audio {
                sample_rate: nz::u32!(48_000),
                channels: nz::u16!(1),
                frame_size: Some(nz::u32!(1024)),
                bit_depth: None,
                timing: AudioTiming::default(),
            }
        })
        .codec_extradata(if video { H264_EXTRADATA } else { AAC_EXTRADATA })
        .build();
        let input = crate::media::fixtures::presentation(vec![track]);
        let plan = if video {
            PlanBuilder::new(0, timebase, nz::u64!(2)).part(nz::u32!(1), nz::u64!(1))
        } else {
            PlanBuilder::new(0, timebase, nz::u64!(8192)).part(nz::u32!(2), nz::u64!(2048))
        };
        let segmentation = SegmentationPlan::new(&input, vec![plan.build()])?;
        let events = discarded_events();
        let mut started = PassThroughMuxerFactory.start(MuxerStartRequest {
            presentation: &input,
            segmentation: &segmentation,
            time_anchor: SystemTime::UNIX_EPOCH,
            events: &events,
        })?;
        let mut publisher = factory(recorder.clone())
            .start(&StreamId::new("camera"), started.presentation.clone())?;
        let frames_per_segment = if video { 2 } else { 8 };
        let mut media = Vec::new();
        for frame in 0..3 * frames_per_segment {
            let sample = if video {
                NormalizedSample::Video(VideoSample {
                    track_id: TrackId(0),
                    codec: Codec::H264,
                    pts: frame,
                    dts: frame,
                    duration: 1,
                    random_access: frame % 2 == 0,
                    payload: Payload::from(if frame % 2 == 0 { H264_IDR } else { H264_P }),
                })
            } else {
                NormalizedSample::Audio(AudioSample {
                    track_id: TrackId(0),
                    codec: Codec::Aac,
                    pts: frame * 1024,
                    duration: 1024,
                    trim: AudioTrim::default(),
                    payload: Payload::from(AAC_FRAME),
                })
            };
            started.muxer.push(sample, &mut media)?;
        }
        started.muxer.finish(FinishReason::Final, &mut media)?;
        for item in media {
            publisher.write(item)?;
        }
        recorder.drain(Duration::from_secs(5)).await;
        let files = temp.files()?;
        assert_eq!(files.len(), 3);
        for file in files {
            let bytes = std::fs::read(&file)?;
            let decoded =
                broadcast_common::Unpackage::unpackage(&mut transmux::Fmp4Demux::new(), &bytes)?;
            assert_eq!(decoded.tracks.len(), 1);
            assert_eq!(
                decoded.tracks[0].samples.len(),
                usize::try_from(frames_per_segment)?
            );
            if video {
                assert!(decoded.tracks[0].samples[0].flags.is_sync);
            }
            // Opt-in independent codec proof; ordinary unit tests need no CLI.
            if std::env::var_os("RUSHLS_TEST_FFMPEG").is_some() {
                let output = std::process::Command::new("ffmpeg")
                    .args(["-v", "error", "-xerror", "-i"])
                    .arg(&file)
                    .args(["-f", "null", "-"])
                    .output()?;
                assert!(
                    output.status.success(),
                    "{}: {}",
                    file.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn filename_collisions_report_failure_without_stopping_publication() -> Result {
    let temp = Temp::new();
    let mut config = temp.config();
    config.pattern = "same.mp4".into();
    let log = Arc::new(Log::default());
    let meters = ProcessMeters::default();
    let recorder = Recorder::start(&config, Events::new(log.clone()), meters.clone())?;
    let mut publisher =
        factory(recorder.clone()).start(&StreamId::new("camera"), presentation())?;
    publisher.write(hls::initialization(0, 1))?;
    for id in 0..2 {
        publisher.write(hls::chunk(0, id, 0, i64::try_from(id)?))?;
        assert_eq!(
            publisher.write(complete(id, i64::try_from(id)?, 1))?,
            PublishOutcome::Published
        );
        recorder.drain(Duration::from_secs(5)).await;
    }
    assert_eq!(temp.files()?.len(), 1);
    // The collision never recovers, so the window opens and stays open: one
    // edge event, and the loss counted once in the meters.
    match log.0.lock().as_slice() {
        [NodeEvent::RecordingFailed { .. }] => {}
        other => panic!("expected a single failure edge: {other:?}"),
    }
    assert_eq!(meters.snapshot().recording_segments_lost, 1);
    assert_eq!(recorder.shared.bytes.load(Ordering::Acquire), 0);
    Ok(())
}
