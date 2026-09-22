//! Apple HLS conformance tests.
//!
//! Each case publishes a fixture through a real ingest path, then hands the
//! resulting origin to `mediastreamvalidator` and `hlsreport`. macOS-only,
//! because both tools are; a machine without them skips rather than fails.
//!
//! Delivery is cleartext, so nothing here needs a keychain or an administrator.
//! [`certs`] explains what that costs and how to opt into TLS.
//!
//! # What the cases are for
//!
//! The ordinary paths — H.264 and AAC over RTMP, MPEG-TS and SRT — are covered
//! first and briefly. Everything after them exists because some property of the
//! input or the cadence is *unusual*, and the comment on each says which one.
//! A case with no such comment is not pulling its weight.

mod certs;
mod export;
mod flv;
mod harness;
mod observe;
mod publish;
mod report;
mod shape;
mod validator;

use std::time::Duration;

use rushls::{
    domain::MediaKind,
    segment::SegmentationPolicy,
    source::transport::srt::{SrtCaller, SrtConfig, SrtListener},
};

use crate::{
    harness::Setup,
    publish::{
        AV1_TS, H264_2398_AAC_TS, H264_AAC_TS, H264_ANAMORPHIC_AAC_TS, H264_DUAL_AAC_TS,
        H264_DUAL_VIDEO_TS, H264_LADDER3_AAC_TS, H264_LONGGOP_AAC_TS, H264_MULTILANG_AAC_TS,
        H264_OPUS_TS, HEVC_AAC_TS, HEVC_HDR10_AAC_TS,
    },
    report::Expect,
    shape::Shape,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// The ordinary paths
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_legacy_h264_aac() -> TestResult {
    run("rtmp_legacy_h264_aac", publish::rtmp_h264_aac()?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_enhanced_hevc_aac() -> TestResult {
    run("rtmp_enhanced_hevc_aac", publish::rtmp_hevc_aac()?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_aac() -> TestResult {
    run("mpegts_h264_aac", publish::mpegts(H264_AAC_TS)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_hevc_aac() -> TestResult {
    run("mpegts_hevc_aac", publish::mpegts(HEVC_AAC_TS)).await
}

// ---------------------------------------------------------------------------
// Picture reordering
// ---------------------------------------------------------------------------

/// A keyframe period that is not a whole number of container ticks: 75 frames
/// of 30000/1001 alternates between 2502 and 2503 milliseconds, so a planned
/// period fixed at either one is wrong every other segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_h264_bframes_aac() -> TestResult {
    run("rtmp_h264_bframes_aac", publish::rtmp_h264_bframes_aac()?).await
}

/// Eight consecutive B-frames under a normal pyramid. Decode order runs seven
/// frame periods behind presentation order, so a segment or part boundary
/// chosen in one is in the wrong place in the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_h264_bpyramid_aac() -> TestResult {
    run("rtmp_h264_bpyramid_aac", publish::rtmp_h264_bpyramid_aac()?).await
}

/// 24000/1001 from a 90 kHz MPEG-TS clock. No frame duration is a whole number
/// of output ticks, so the encoded run overshoots the planned cut.
///
/// The window that would absorb it is not a policy setting: a video track gets
/// a non-zero `boundary_tolerance` only when cadence derivation produced an
/// exact `segment_period`, and a rate whose period is fractional in the source
/// timebase does not. `early_boundary` and `late_boundary` are about *which*
/// random access point to cut at, not about how far an encoded run may overrun
/// the one that was chosen, so no cadence avoids this. The sibling below runs
/// the same input at Apple's recommended cadence to show exactly that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_23_976() -> TestResult {
    run("mpegts_h264_23_976", publish::mpegts(H264_2398_AAC_TS)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_23_976_at_apple_cadence() -> TestResult {
    run_with(
        Setup::default()
            .named("mpegts_h264_23_976_at_apple_cadence")
            .segmentation(SegmentationPolicy::latency_first(
                Duration::from_secs(6),
                Duration::from_secs(1),
            ))
            .expect(Expect::apple_cadence),
        publish::mpegts(H264_2398_AAC_TS),
    )
    .await
}

// ---------------------------------------------------------------------------
// Audio shapes
// ---------------------------------------------------------------------------

/// HE-AAC. The decoded output rate is twice the encoded frame rate, so the
/// sample entry, the `SAMPLE-RATE` attribute, and the `mp4a.40.5` codec string
/// all have to agree about which of the two they are describing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_h264_he_aac() -> TestResult {
    run("rtmp_h264_he_aac", publish::rtmp_h264_he_aac()?).await
}

/// 44.1 kHz mono: the audio grid shares no common period with a 30 fps video
/// timescale, so no boundary is exact in both and the trim has to absorb it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_h264_aac_441_mono() -> TestResult {
    run("rtmp_h264_aac_441_mono", publish::rtmp_h264_aac_441_mono()?).await
}

/// 5.1, which the audio rendition must advertise as `CHANNELS="6"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_h264_aac_51() -> TestResult {
    run("rtmp_h264_aac_51", publish::rtmp_h264_aac_51()?).await
}

/// Opus, which this origin accepts and Apple's HLS profile does not list.
///
/// Apple refusing to play it is not a defect and is not what this case is for.
/// What it checks is that everything *else* about the presentation is still
/// right: the playlists parse, the segments fetch, the cadence holds. A
/// packager that quietly mangles a codec its own tests accept would otherwise
/// be indistinguishable from one that packaged it correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_opus() -> TestResult {
    run_with(
        Setup::default()
            .named("mpegts_h264_opus")
            .expect(Expect::unlisted_codec),
        publish::mpegts(H264_OPUS_TS),
    )
    .await
}

/// AV1 with no audio at all, through the GStreamer AV1G MPEG-TS mapping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_av1_video_only() -> TestResult {
    run_with(
        Setup::default()
            .named("mpegts_av1_video_only")
            .expect(|expect| expect.unlisted_codec().video_only())
            // 10 fps with a two-frame GOP: exercise frequent random-access
            // points in one-second segments without an audio clock.
            .segmentation(SegmentationPolicy::latency_first(
                Duration::from_secs(1),
                Duration::from_millis(500),
            )),
        publish::mpegts(AV1_TS),
    )
    .await
}

// ---------------------------------------------------------------------------
// Presentation topologies
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_multitrack_h264_two_aac() -> TestResult {
    run(
        "rtmp_multitrack_h264_two_aac",
        publish::rtmp_h264_two_aac()?,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_two_aac() -> TestResult {
    run("mpegts_h264_two_aac", publish::mpegts(H264_DUAL_AAC_TS)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_two_video() -> TestResult {
    run_with(
        Setup::default()
            .named("mpegts_h264_two_video")
            .expect(Expect::ladder),
        publish::mpegts(H264_DUAL_VIDEO_TS),
    )
    .await
}

/// Three video renditions with aligned key frames — a real ladder, which is
/// what makes `EXT-X-STREAM-INF` ordering, bandwidth, and the shared audio
/// group answerable rather than trivial.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_three_rendition_ladder() -> TestResult {
    run_with(
        Setup::default()
            .named("mpegts_h264_three_rendition_ladder")
            .expect(Expect::ladder),
        publish::mpegts(H264_LADDER3_AAC_TS),
    )
    .await
}

/// Two audio renditions whose languages come from the PMT rather than from a
/// test stamping them on. Apple requires autoselected languages to be distinct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_two_audio_languages() -> TestResult {
    run(
        "mpegts_h264_two_audio_languages",
        publish::mpegts(H264_MULTILANG_AAC_TS),
    )
    .await
}

/// Audio with no video track at all. The presentation has no variant to hang a
/// rendition group off, so the audio has to become the variant itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_audio_only() -> TestResult {
    run_with(
        Setup::default()
            .named("rtmp_audio_only")
            .expect(Expect::audio_only),
        publish::rtmp_shaped_h264_aac(Shape::labelled().without(MediaKind::Video))?,
    )
    .await
}

/// Video with no audio track. Nothing snaps the segment boundary to an audio
/// grid, and no rendition group is declared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_video_only() -> TestResult {
    run(
        "rtmp_video_only",
        publish::rtmp_shaped_h264_aac(Shape::labelled().without(MediaKind::Audio))?,
    )
    .await
}

// ---------------------------------------------------------------------------
// Subtitles
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_captions_webvtt() -> TestResult {
    run_with(
        Setup::default()
            .named("rtmp_captions_webvtt")
            .expect(Expect::captions),
        publish::rtmp_h264_aac_captions()?,
    )
    .await
}

/// Three subtitle renditions in one group.
///
/// No ingest adapter can produce this today — RTMP script data yields at most
/// one subtitle track and the other transports yield none — so the catalog is
/// reshaped above the adapter. See [`shape`]. The projection, the WebVTT muxer,
/// and the store all claim to handle a group with several members, and this is
/// what asks Apple whether they do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_three_subtitle_renditions() -> TestResult {
    run_with(
        Setup::default()
            .named("rtmp_three_subtitle_renditions")
            .expect(Expect::captions),
        publish::rtmp_shaped_h264_aac_captions(
            Shape::labelled().subtitle_languages(&["sv", "de"]),
        )?,
    )
    .await
}

/// A subtitle group beside an audio group beside a video ladder: the fullest
/// presentation this origin can currently express.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtmp_subtitles_and_two_audio_languages() -> TestResult {
    run_with(
        Setup::default()
            .named("rtmp_subtitles_and_two_audio_languages")
            .expect(Expect::captions),
        publish::rtmp_shaped_h264_aac_captions(
            Shape::labelled()
                .subtitle_languages(&["sv"])
                .audio_languages(&["sv"]),
        )?,
    )
    .await
}

// ---------------------------------------------------------------------------
// Colour and geometry
// ---------------------------------------------------------------------------

/// HDR10: BT.2020 primaries, the PQ transfer function, and mastering-display
/// SEI. The multivariant playlist must answer `VIDEO-RANGE=PQ`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_hevc_hdr10() -> TestResult {
    run("mpegts_hevc_hdr10", publish::mpegts(HEVC_HDR10_AAC_TS)).await
}

/// SAR 4:3, so encoded and displayed geometry differ. `RESOLUTION` is the
/// encoded one; advertising the display size would name a rendition that does
/// not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegts_h264_anamorphic() -> TestResult {
    run(
        "mpegts_h264_anamorphic",
        publish::mpegts(H264_ANAMORPHIC_AAC_TS),
    )
    .await
}

// ---------------------------------------------------------------------------
// Cadence
//
// Segmentation is where an origin most easily produces a playlist that parses
// and still cannot be played, so these vary the policy rather than the media.
// ---------------------------------------------------------------------------

/// Apple's own recommendation: six-second segments and one-second parts. Slower
/// than the rest of the suite, and the only case entitled to be judged against
/// the target-duration guidance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_apple_recommended() -> TestResult {
    run_with(
        Setup::default()
            .named("cadence_apple_recommended")
            .segmentation(SegmentationPolicy::latency_first(
                Duration::from_secs(6),
                Duration::from_secs(1),
            ))
            .expect(Expect::apple_cadence),
        publish::mpegts(H264_AAC_TS),
    )
    .await
}

/// A part target that does not divide the segment target: 2 s of segment in
/// 0.7 s parts leaves a remainder every time, and the last part of each segment
/// is the one that has to absorb it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_part_target_does_not_divide_segment() -> TestResult {
    run_with(
        Setup::default()
            .named("cadence_part_target_does_not_divide_segment")
            .segmentation(SegmentationPolicy::latency_first(
                Duration::from_secs(2),
                Duration::from_millis(700),
            )),
        publish::mpegts(H264_AAC_TS),
    )
    .await
}

/// Parts shorter than a video frame period is long. At 30 fps a 0.2 s part is
/// six frames, and the 85% floor leaves almost no room to place a cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_very_short_parts() -> TestResult {
    run_with(
        Setup::default()
            .named("cadence_very_short_parts")
            .segmentation(SegmentationPolicy::latency_first(
                Duration::from_secs(1),
                Duration::from_millis(200),
            )),
        publish::mpegts(H264_AAC_TS),
    )
    .await
}

/// A desired segment shorter than the key frame interval, with a maximum that
/// can still hold one. There is no random access point where the policy wants
/// one, so the maximum decides and the advertised target duration has to match
/// the five-second segments that were actually produced rather than the
/// two-second ones that were asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_segment_target_below_gop() -> TestResult {
    run_with(
        Setup::default()
            .named("cadence_segment_target_below_gop")
            .segmentation(harness::segmentation_with_headroom(
                Duration::from_secs(2),
                Duration::from_millis(500),
                Duration::from_secs(6),
            )),
        // A five-second GOP against a two-second desire.
        publish::mpegts(H264_LONGGOP_AAC_TS),
    )
    .await
}

/// The same long GOP with no headroom at all: the maximum is four seconds and
/// the encoder offers a boundary every five, so no legal cut exists.
///
/// Refusing is the right answer — an origin cannot invent a random access point
/// — and what this pins is that it is refused *cleanly*, before anything is
/// published, rather than by serving a playlist no client can start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_no_boundary_within_the_maximum_is_refused() -> TestResult {
    let Some(origin) = harness::Origin::start_with(
        Setup::default()
            .named("cadence_no_boundary_within_the_maximum_is_refused")
            .segmentation(SegmentationPolicy::latency_first(
                Duration::from_secs(2),
                Duration::from_millis(500),
            )),
    )
    .await?
    else {
        return Ok(());
    };
    let error = origin
        .publish(publish::mpegts(H264_LONGGOP_AAC_TS))
        .await
        .expect_err("a five-second GOP has no boundary within a four-second maximum");
    let message = error.to_string();
    assert!(
        message.contains("random-access boundary"),
        "the refusal must say which cadence it could not satisfy: {message}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn srt_h264_aac() -> TestResult {
    let Some(origin) = harness::Origin::start_with(Setup::default().named("srt_h264_aac")).await?
    else {
        return Ok(());
    };

    let config = SrtConfig::default();
    let mut listener = SrtListener::bind("127.0.0.1:0".parse()?, config.clone(), 4).await?;
    let address = listener.local_address();
    let caller = tokio::spawn(async move {
        let caller = SrtCaller::connect(address, &config, "publish:live/camera:secret")
            .await
            .map_err(|error| error.to_string())?;
        caller
            .send_mpegts(H264_AAC_TS)
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(caller)
    });

    let pending = listener
        .accept()
        .await
        .ok_or("SRT listener closed")?
        .map_err(|error| error.to_string())?;
    // This finite publisher must close before validation. Leaving an idle
    // socket open makes blocking reloads wait for media that will never arrive.
    let session = origin.spawn_session(publish::labeled(Box::new(pending)));
    drop(caller.await.map_err(|_| "SRT caller panicked")??);
    match session.await? {
        Ok(rushls::session::SessionOutcome::Ended) => {}
        Ok(other) => return Err(format!("SRT session ended unexpectedly: {other:?}").into()),
        Err(error) => return Err(error.into()),
    }
    assert!(
        origin.reported_failures().is_empty(),
        "{:?}",
        origin.reported_failures()
    );
    origin.validate_playlist()
}

// ---------------------------------------------------------------------------
// Playlist finalization
//
// Separate from the conformance cases because the defect it catches makes every
// other case fail in a way that hides its own cause: a playlist that never
// finalizes leaves Apple blocking on a reload that never returns, and each of
// its media playlists is then reported as unparseable.
// ---------------------------------------------------------------------------

/// A publication that ends must end its playlists.
///
/// A media playlist with no `EXT-X-ENDLIST` says more media is coming. When
/// none is, every client polls forever and a blocking reload cannot be answered
/// — which the origin reports as 503. A `EXT-X-PRELOAD-HINT` left behind names
/// a part that will never exist, so the client's next fetch is parked until it
/// times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graceful_publication_finalizes_every_playlist() -> TestResult {
    for (label, pending) in [
        ("rtmp", publish::rtmp_h264_aac()?),
        ("mpeg-ts", publish::mpegts(H264_AAC_TS)),
    ] {
        let Some(origin) = harness::Origin::start().await? else {
            return Ok(());
        };
        origin.publish(pending).await?;
        // A session that could not drain still reports \`Ended\`, so the outcome
        // alone cannot distinguish a finalized publication from an abandoned
        // one. Read what the origin said about itself instead.
        assert!(
            origin.reported_failures().is_empty(),
            "{label}: a graceful end reported failures: {:?}",
            origin.reported_failures()
        );
        for (uri, playlist) in origin.media_playlists()? {
            assert!(
                playlist.contains("#EXT-X-ENDLIST"),
                "{label}: a graceful end must finalize {uri}:\n{playlist}"
            );
            assert!(
                !playlist.contains("#EXT-X-PRELOAD-HINT"),
                "{label}: a finalized playlist must not hint media that will \
                 never arrive, {uri}:\n{playlist}"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

async fn run(name: &'static str, pending: Box<dyn rushls::source::PendingPublish>) -> TestResult {
    run_with(Setup::default().named(name), pending).await
}

async fn run_with(setup: Setup, pending: Box<dyn rushls::source::PendingPublish>) -> TestResult {
    let Some(origin) = harness::Origin::start_with(setup).await? else {
        return Ok(());
    };
    origin.publish_and_validate(pending).await
}

/// Prints every playlist a completed publication serves.
///
/// Kept because the conformance tools report *that* a playlist was rejected far
/// more clearly than they report what was in it, and this is the fastest way to
/// see the difference between two inputs that behave differently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic: prints the playlists a completed publication serves"]
async fn dump_playlists() -> TestResult {
    for (label, pending) in [
        ("rtmp_h264_aac", publish::rtmp_h264_aac()?),
        ("rtmp_h264_bpyramid_aac", publish::rtmp_h264_bpyramid_aac()?),
        ("mpegts_h264_aac", publish::mpegts(H264_AAC_TS)),
    ] {
        let Some(origin) = harness::Origin::start().await? else {
            return Ok(());
        };
        eprintln!("\n########## {label} ##########");
        eprintln!("session outcome: {:?}", origin.publish(pending).await?);
        eprintln!(
            "--- multivariant ---\n{}",
            validator::fetch(origin.url(), origin.certificate_authority())?
        );
        for (uri, playlist) in origin.media_playlists()? {
            eprintln!("--- {uri} ---\n{playlist}");
        }
    }
    Ok(())
}

// Matched controls retain identical encoded input; loss occurs before normalization.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_audio_only_with_control() -> TestResult {
    gap_case(
        "gap_audio_only",
        vec![MediaKind::Audio],
        Some(MediaKind::Video),
    )
    .await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_video_with_audio_control() -> TestResult {
    gap_case("gap_video_with_audio", vec![MediaKind::Video], None).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_overlapping_av_with_control() -> TestResult {
    gap_case(
        "gap_overlapping_av",
        vec![MediaKind::Video, MediaKind::Audio],
        None,
    )
    .await
}

// Packaging remains testable even though native Safari continuation is a known failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_video_only_with_control() -> TestResult {
    gap_case(
        "gap_video_only",
        vec![MediaKind::Video],
        Some(MediaKind::Audio),
    )
    .await
}

async fn gap_case(
    name: &'static str,
    holes: Vec<MediaKind>,
    drop: Option<MediaKind>,
) -> TestResult {
    const FIXTURE: &[u8] = include_bytes!("fixtures/h264_fixed_aac.ts");
    for damaged in [false, true] {
        let setup = Setup::default().named(name).expect(|e| match drop {
            Some(MediaKind::Video) => e.audio_only(),
            Some(MediaKind::Audio) => e.video_only(),
            _ => e,
        });
        let Some(origin) = harness::Origin::start_with(setup).await? else {
            return Ok(());
        };
        let shape = Shape {
            holes: if damaged { holes.clone() } else { Vec::new() },
            drop: drop.into_iter().collect(),
            ..Shape::labelled()
        };
        origin
            .publish(publish::mpegts_shaped(FIXTURE, shape))
            .await?;
        assert!(origin.reported_failures().is_empty());
        let playlists = origin.media_playlists()?;
        let mut gap_count = 0;
        for (_, playlist) in &playlists {
            gap_count += playlist.matches("#EXT-X-GAP\n").count();
            assert!(!playlist.contains("#EXT-X-DISCONTINUITY"));
            assert!(playlist.contains("#EXT-X-ENDLIST"));
            assert!(!playlist.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        }
        assert_eq!(
            gap_count,
            if damaged { holes.len() } else { 0 },
            "loss must reach publication, not merely make a permissive test pass"
        );
        origin.validate_playlist()?;
        if let Some(directory) = std::env::var_os("RUSHLS_TEST_GAP_EXPORT_DIR") {
            let case = format!("{name}{}", if damaged { "" } else { "-control" });
            export::save(
                origin.url(),
                &std::path::PathBuf::from(directory).join(case),
            )?;
        }
    }
    Ok(())
}
