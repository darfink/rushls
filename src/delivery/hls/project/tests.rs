//! Projection tests, driven through a real store rather than hand-built
//! snapshots.

use std::{sync::Arc, time::Duration};

use crate::{
    delivery::hls::{
        RenditionSnapshot, RetentionPolicy, StoreLimits, StreamLease, StreamSnapshot, StreamStore,
        fixtures::{
            audio, chunk, initialization, lease, stream_id, subtitle, subtitle_with_parts, video,
            write, write_direct, write_segment,
        },
        project::{
            DeliveryTimingPolicy, PlaylistDelta, PlaylistPolicy, ProgramDateTimePolicy,
            media::media_playlist, multivariant::multivariant_playlist,
            presentation_server_control,
        },
        uri::{PlaylistUris, UriBase},
    },
    domain::RenditionId,
    mux::{CaptionChannel, ClosedCaptionService},
};

fn policy() -> PlaylistPolicy {
    PlaylistPolicy::default()
}

/// The default: every name relative to the playlist that emits it.
fn uris() -> PlaylistUris {
    UriBase::default().uris(&stream_id())
}

fn snapshots(
    lease: &StreamLease,
    rendition: u32,
) -> Result<(Arc<StreamSnapshot>, Arc<RenditionSnapshot>), Box<dyn std::error::Error>> {
    let stream = lease.live().snapshot();
    let media = lease
        .live()
        .rendition(RenditionId(rendition))
        .ok_or_else(|| std::io::Error::other("the rendition is in the catalog"))?;
    Ok((stream, media))
}

fn render(
    lease: &StreamLease,
    rendition: u32,
    policy: &PlaylistPolicy,
) -> Result<String, Box<dyn std::error::Error>> {
    let (stream, media) = snapshots(lease, rendition)?;
    let control = presentation_server_control(&stream, DeliveryTimingPolicy::default());
    Ok(media_playlist(
        &stream,
        &media,
        control,
        policy,
        &uris(),
        PlaylistDelta::Full,
    )?)
}

fn render_delta(
    lease: &StreamLease,
    rendition: u32,
    policy: &PlaylistPolicy,
    delta: PlaylistDelta,
) -> Result<String, Box<dyn std::error::Error>> {
    let (stream, media) = snapshots(lease, rendition)?;
    let control = presentation_server_control(&stream, DeliveryTimingPolicy::default());
    Ok(media_playlist(
        &stream,
        &media,
        control,
        policy,
        &uris(),
        delta,
    )?)
}

#[test]
fn a_live_playlist_states_its_terms_before_any_media() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let rendered = render(&lease, 0, &policy())?;
    let header: Vec<&str> = rendered
        .lines()
        .take_while(|line| !line.starts_with("#EXT-X-MAP"))
        .collect();

    assert_eq!(
        header,
        vec![
            "#EXTM3U",
            "#EXT-X-VERSION:9",
            "#EXT-X-TARGETDURATION:6",
            "#EXT-X-SERVER-CONTROL:HOLD-BACK=18,PART-HOLD-BACK=3.000001,CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=36",
            "#EXT-X-PART-INF:PART-TARGET=1",
            "#EXT-X-MEDIA-SEQUENCE:0",
        ],
        "the target, the part target, and the hold-backs all come from the \
         locked plan, so they are known before a single segment exists"
    );
    assert!(
        !rendered.contains("CAN-SKIP-DATERANGES"),
        "CAN-SKIP-DATERANGES=NO is not a spec value; omit it"
    );
    Ok(())
}

#[test]
fn a_completed_segment_carries_its_map_and_its_parts() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let rendered = render(&lease, 0, &policy())?;

    assert!(rendered.contains("#EXT-X-MAP:URI=\"init/1.mp4\"\n"));
    assert!(rendered.contains("#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:20Z\n"));
    assert!(rendered.contains(
        "#EXT-X-PART:DURATION=1,URI=\"part/1.m4s\",INDEPENDENT=YES\n\
         #EXT-X-PART:DURATION=1,URI=\"part/2.m4s\"\n"
    ));
    assert!(rendered.contains("#EXTINF:6,\nsegment/1.m4s\n"));
    assert_eq!(
        rendered.matches("#EXT-X-MAP").count(),
        1,
        "the map is restated only where the initialization changes"
    );
    Ok(())
}

#[test]
fn an_open_segment_is_described_before_its_first_part_is_offered()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write_segment(&first, 0, 0, 0);

    // A takeover: the successor's open segment is a splice, and its parts are
    // fetchable long before the segment it belongs to completes.
    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    write(&second, chunk(0, 0, 0, 0));

    let rendered = render(&second, 0, &policy())?;
    let leading_up_to_the_part = rendered
        .split_once("#EXT-X-PART:DURATION=1,URI=\"part/7.m4s\"")
        .ok_or_else(|| std::io::Error::other("the successor's first part is tagged"))?
        .0;
    let leading: Vec<&str> = leading_up_to_the_part.lines().rev().take(3).collect();

    assert_eq!(
        leading,
        vec![
            "#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:20Z",
            "#EXT-X-MAP:URI=\"init/2.mp4\"",
            "#EXT-X-DISCONTINUITY",
        ],
        "a client fetching this part must already know it decodes against a new \
         initialization on a new timeline"
    );
    Ok(())
}

#[test]
fn a_departed_discontinuity_survives_as_a_sequence_number() -> Result<(), Box<dyn std::error::Error>>
{
    // A six-segment window, so the seventh publication below genuinely pushes
    // the discontinuity out of it. Pinned rather than defaulted: what is under
    // test is what survives departure, not how wide a shipped `retain` is.
    let store = StreamStore::new(crate::delivery::hls::StoreLimits {
        retention: crate::delivery::hls::RetentionPolicy {
            retain: std::time::Duration::from_secs(36).into(),
            ..crate::delivery::hls::RetentionPolicy::default()
        },
        ..crate::delivery::hls::StoreLimits::default()
    });
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write(&first, chunk(0, 0, 0, 0));

    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    for id in 0..7 {
        write_segment(
            &second,
            0,
            id,
            i64::try_from(id).expect("fixture id fits i64") * 6,
        );
    }

    let rendered = render(&second, 0, &policy())?;

    assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
    assert!(rendered.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
    assert!(
        !rendered.contains("#EXT-X-DISCONTINUITY\n"),
        "the tag left with its segment; the sequence number is what remains"
    );
    Ok(())
}

#[test]
fn a_preload_hint_names_the_part_that_does_not_exist_yet() -> Result<(), Box<dyn std::error::Error>>
{
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write(&lease, chunk(0, 0, 0, 0));

    let rendered = render(&lease, 0, &policy())?;

    assert!(rendered.contains("#EXT-X-PART:DURATION=1,URI=\"part/1.m4s\",INDEPENDENT=YES\n"));
    assert!(rendered.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/2.m4s\"\n"));
    assert!(!rendered.contains("#EXT-X-ENDLIST"));
    Ok(())
}

#[test]
fn ending_a_publication_closes_every_playlist_and_withdraws_the_hint()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    lease.end();

    let rendered = render(&lease, 0, &policy())?;

    assert!(rendered.ends_with("#EXT-X-ENDLIST\n"));
    assert!(
        !rendered.contains("PRELOAD-HINT"),
        "hinting media after an ENDLIST would park a client on a fetch that \
         can never be satisfied"
    );
    Ok(())
}

#[test]
fn siblings_are_reported_so_a_switching_client_knows_where_to_resume()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }
    write(&lease, chunk(1, 1, 0, 6));

    let rendered = render(&lease, 0, &policy())?;

    assert!(
        rendered
            .contains("#EXT-X-RENDITION-REPORT:URI=\"../1/audio.m3u8\",LAST-MSN=1,LAST-PART=0\n"),
        "the audio rendition has a part open in MSN 1, which is where a client \
         switching to it should ask to continue: {rendered}"
    );
    assert!(
        !rendered.contains("../0/video.m3u8"),
        "a playlist does not report itself"
    );
    Ok(())
}

#[test]
fn query_variables_raise_the_version_and_suffix_every_uri() -> Result<(), Box<dyn std::error::Error>>
{
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }

    let uris = uris().with_query_variables();
    let (stream, media) = snapshots(&lease, 0)?;
    let control = presentation_server_control(&stream, DeliveryTimingPolicy::default());
    let rendered = media_playlist(
        &stream,
        &media,
        control,
        &policy(),
        &uris,
        PlaylistDelta::Full,
    )?;
    let multivariant = multivariant_playlist(&stream, &policy(), &uris)?.ok_or("topology")?;

    assert!(
        rendered.contains("#EXT-X-VERSION:11\n"),
        "QUERYPARAM requires protocol version 11: {rendered}"
    );
    assert!(rendered.contains("#EXT-X-DEFINE:QUERYPARAM=\"token\"\n"));
    assert!(rendered.contains("URI=\"init/1.mp4?token={$token}\""));
    assert!(rendered.contains("segment/1.m4s?token={$token}"));
    assert!(rendered.contains("URI=\"../1/audio.m3u8?token={$token}\""));
    assert!(
        !rendered.contains("token=eyJ"),
        "the playlist names the variable, not a viewer token: {rendered}"
    );
    assert!(multivariant.contains("#EXT-X-VERSION:11\n"));
    assert!(multivariant.contains("#EXT-X-DEFINE:QUERYPARAM=\"token\"\n"));
    assert!(
        multivariant.contains("0/video.m3u8?token={$token}"),
        "STREAM-INF names the query-variable playlist: {multivariant}"
    );
    Ok(())
}

#[test]
fn a_cueless_subtitle_rendition_is_still_reported_to_its_siblings()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), subtitle(1)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    // Only what the cadence heartbeat produces: an empty segment, no cue.
    write(&lease, initialization(1, 1));
    write_direct(&lease, 1, 0, 0);

    let rendered = render(&lease, 0, &policy())?;

    assert!(
        rendered.contains("#EXT-X-RENDITION-REPORT:URI=\"../1/subtitles.m3u8\",LAST-MSN=0\n"),
        "a client toggling subtitles on needs somewhere to resume even before \
         the first cue exists, and a part-less rendition reports no LAST-PART: \
         {rendered}"
    );
    Ok(())
}

#[test]
fn a_configured_base_makes_every_name_absolute() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }

    let (stream, media) = snapshots(&lease, 0)?;
    let control = presentation_server_control(&stream, DeliveryTimingPolicy::default());
    let uris = UriBase::new("https://cdn.example.com/hls").uris(&stream_id());
    let rendered = media_playlist(
        &stream,
        &media,
        control,
        &policy(),
        &uris,
        PlaylistDelta::Full,
    )?;
    let multivariant = multivariant_playlist(&stream, &policy(), &uris)?
        .ok_or_else(|| std::io::Error::other("an attached publication has a topology"))?;

    assert!(
        rendered
            .contains("#EXT-X-MAP:URI=\"https://cdn.example.com/hls/live/camera/0/init/1.mp4\"\n")
    );
    assert!(
        rendered.contains("#EXTINF:6,\nhttps://cdn.example.com/hls/live/camera/0/segment/1.m4s\n")
    );
    assert!(
        rendered.contains("URI=\"https://cdn.example.com/hls/live/camera/1/audio.m3u8\""),
        "a sibling is reached through the base rather than by stepping out of \
         a directory that a rooted name never entered: {rendered}"
    );
    assert!(multivariant.contains("\nhttps://cdn.example.com/hls/live/camera/0/video.m3u8\n"));
    Ok(())
}

#[test]
fn a_webvtt_playlist_names_vtt_resources_and_advertises_no_parts()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), subtitle(1)]);
    write(&lease, initialization(1, 1));
    write_direct(&lease, 1, 0, 0);

    let rendered = render(&lease, 1, &policy())?;

    assert!(rendered.contains("#EXT-X-VERSION:6\n"));
    assert!(rendered.contains("#EXT-X-MAP:URI=\"init/1.vtt\"\n"));
    assert!(rendered.contains("#EXTINF:6,\nsegment/1.vtt\n"));
    assert!(
        !rendered.contains("#EXT-X-PART-INF"),
        "a segment-only rendition has no part target to advertise"
    );
    assert!(
        rendered.contains("PART-HOLD-BACK=3"),
        "but it still carries the presentation-wide server control, which its \
         chunked sibling requires"
    );
    Ok(())
}

#[test]
fn a_chunked_webvtt_playlist_advertises_vtt_parts() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), subtitle_with_parts(1)]);
    write(&lease, initialization(1, 1));
    write_segment(&lease, 1, 0, 0);
    write(&lease, chunk(1, 1, 0, 6));

    let rendered = render(&lease, 1, &policy())?;

    assert!(
        rendered.contains("#EXT-X-PART-INF:PART-TARGET=1\n"),
        "a subtitle rendition that publishes parts declares its grid like any \
         other: {rendered}"
    );
    assert!(
        rendered.contains("#EXT-X-PART:DURATION=1,URI=\"part/1.vtt\""),
        "a WebVTT part is a WebVTT resource, not an fMP4 one: {rendered}"
    );
    assert!(rendered.contains("#EXT-X-MAP:URI=\"init/1.vtt\"\n"));
    assert!(
        rendered.contains("#EXTINF:6,\nsegment/1.vtt\n"),
        "the parent stays addressable as a whole segment: {rendered}"
    );
    Ok(())
}

#[test]
fn completed_webvtt_parts_remain_tagged_through_the_retention_frontier()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), subtitle_with_parts(1)]);
    write(&lease, initialization(1, 1));
    for segment in 0..4 {
        write_segment(&lease, 1, segment, i64::try_from(segment)? * 6);
    }

    let retained = render(&lease, 1, &policy())?;
    assert!(
        retained.contains("#EXT-X-PART:DURATION=1,URI=\"part/1.vtt\""),
        "the oldest subtitle parent's parts remain tagged at exactly three \
         target durations from the live edge: {retained}"
    );

    write(&lease, chunk(1, 4, 0, 24));
    let advanced = render(&lease, 1, &policy())?;
    assert!(
        !advanced.contains("URI=\"part/1.vtt\""),
        "the oldest subtitle parent's parts leave together only after crossing \
         the retention frontier: {advanced}"
    );
    assert!(
        advanced.contains("URI=\"part/7.vtt\""),
        "newer completed subtitle parts remain advertised: {advanced}"
    );
    Ok(())
}

#[test]
fn a_gap_is_tagged_rather_than_omitted() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write(&first, chunk(0, 0, 0, 0));

    // Takeover strands the open segment, which the store resolves as a gap so
    // the MSN a client already saw does not simply vanish.
    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    write(&second, chunk(0, 0, 0, 0));

    let rendered = render(&second, 0, &policy())?;

    assert!(rendered.contains("#EXT-X-GAP\n#EXTINF:6,\nsegment/1.m4s\n"));
    Ok(())
}

#[test]
fn program_date_time_can_be_restated_on_every_segment() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    write_segment(&lease, 0, 1, 6);

    let sparse = render(&lease, 0, &policy())?;
    let dense = render(
        &lease,
        0,
        &PlaylistPolicy {
            program_date_time: ProgramDateTimePolicy::EverySegment,
            ..policy()
        },
    )?;

    assert_eq!(sparse.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 1);
    assert_eq!(dense.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 2);
    assert!(dense.contains("#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:26Z\n"));
    Ok(())
}

#[test]
fn a_variant_advertises_what_playing_it_actually_costs() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1), subtitle(2)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }
    write(&lease, initialization(2, 1));
    write_direct(&lease, 2, 0, 0);

    let stream = lease.live().snapshot();
    let rendered = multivariant_playlist(&stream, &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("an attached publication has a topology"))?;

    let rate = |index: usize| -> Result<_, std::io::Error> {
        stream.renditions[index]
            .bandwidth
            .peak_bits_per_second
            .ok_or_else(|| std::io::Error::other("a completed segment has been measured"))
    };

    assert!(
        rendered.contains(&format!("BANDWIDTH={}", rate(0)? + rate(1)? + rate(2)?)),
        "the advertised rate is the primary plus one selectable rendition from \
         each group the variant references, not the video rate alone: {rendered}"
    );
    assert!(rendered.contains("AUDIO=\"audio\""));
    assert!(rendered.contains("SUBTITLES=\"subtitle\""));
    assert!(rendered.contains("\n0/video.m3u8\n"));
    assert!(rendered.contains("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\""));
    assert!(rendered.contains("URI=\"1/audio.m3u8\""));
    assert!(
        rendered.contains("URI=\"2/subtitles.m3u8\""),
        "a playlist's file name says what it carries: {rendered}"
    );
    assert!(
        !rendered.contains("#EXT-X-MEDIA:TYPE=VIDEO"),
        "the primary group's renditions are the variants themselves"
    );
    Ok(())
}

#[test]
fn a_multivariant_playlist_preserves_canonical_language_metadata()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let mut french = audio(1);
    french.language = Some(Arc::from("fr-CA"));
    let lease = lease(&store, vec![video(0), french]);

    let rendered = multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("an attached publication has a topology"))?;

    assert!(rendered.contains("LANGUAGE=\"fr-CA\""));
    Ok(())
}

#[test]
fn detected_captions_are_declared_once_and_referenced_by_every_variant()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);

    // Before detection the presentation says nothing about captions, which is
    // legal: the attribute is optional, and this origin only knows what it has
    // observed.
    let before = multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("an attached publication has a topology"))?;
    assert!(!before.contains("CLOSED-CAPTIONS"));

    assert!(
        lease.declare_closed_captions(Arc::from([ClosedCaptionService {
            channel: CaptionChannel::Cea708Service(1),
            name: Arc::from("Service 1"),
            language: Some(Arc::from("en")),
            is_default: true,
            autoselect: true,
        }]))
    );

    let after = multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("an attached publication has a topology"))?;

    assert!(after.contains(concat!(
        "#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"Service 1\",",
        "DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"en\",INSTREAM-ID=\"SERVICE1\"\n"
    )));
    // Section 4.4.6.1 forbids a URI on a closed-caption rendition, so the tag
    // must be the whole declaration.
    assert!(!after.contains("INSTREAM-ID=\"SERVICE1\",URI"));

    // Section 4.4.6.2 requires every variant to agree, so the count of
    // references must match the count of variants rather than merely be
    // nonzero.
    let variants = after.matches("#EXT-X-STREAM-INF:").count();
    assert_eq!(after.matches("CLOSED-CAPTIONS=\"cc\"").count(), variants);
    Ok(())
}

#[test]
fn a_caption_channel_hls_cannot_name_is_left_out_of_the_playlist()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);

    // Service 64 is outside the 1..=63 the spec permits. Emitting it would
    // leave a variant referencing a group that was never written.
    assert!(
        lease.declare_closed_captions(Arc::from([ClosedCaptionService {
            channel: CaptionChannel::Cea708Service(64),
            name: Arc::from("Service 64"),
            language: None,
            is_default: true,
            autoselect: true,
        }]))
    );

    let rendered = multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("an attached publication has a topology"))?;

    assert!(!rendered.contains("CLOSED-CAPTIONS"));
    assert!(!rendered.contains("TYPE=CLOSED-CAPTIONS"));
    Ok(())
}

#[test]
fn an_unmeasured_presentation_is_servable_immediately() -> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);

    let stream = lease.live().snapshot();
    let rendered = multivariant_playlist(&stream, &policy(), &uris())?.ok_or_else(|| {
        std::io::Error::other("a topology exists as soon as a publisher attaches")
    })?;

    assert!(
        rendered.contains(&format!("BANDWIDTH={}", 6_000_000 * 2)),
        "with nothing measured or declared, both renditions fall back to the \
         configured assumption so startup is not delayed by a segment: {rendered}"
    );
    assert!(
        !rendered.contains("AVERAGE-BANDWIDTH"),
        "an average nobody has measured would describe nothing"
    );
    Ok(())
}

#[test]
fn an_optimistic_declaration_is_not_lowered_by_a_smaller_measurement()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let mut declared = video(0);
    declared.declared_bandwidth = Some(9_000_000);
    let lease = lease(&store, vec![declared]);

    let before = multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("has a topology"))?;
    assert!(before.contains("BANDWIDTH=9000000"));

    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    let stream = lease.live().snapshot();
    let measured = stream.renditions[0]
        .bandwidth
        .peak_bits_per_second
        .ok_or_else(|| std::io::Error::other("a completed segment has been measured"))?;

    let after = multivariant_playlist(&stream, &policy(), &uris())?
        .ok_or_else(|| std::io::Error::other("has a topology"))?;
    assert!(
        after.contains("BANDWIDTH=9000000"),
        "under-advertising is what makes a client pick a variant it cannot \
         sustain, so the larger of the two wins: measured {measured}"
    );
    Ok(())
}

#[test]
fn a_stream_nobody_has_published_to_has_no_presentation() -> Result<(), Box<dyn std::error::Error>>
{
    let store = StreamStore::default();
    let lease = store.lease_without_presentation(stream_id())?;

    assert!(multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())?.is_none());
    Ok(())
}

fn long_window() -> StreamStore {
    StreamStore::new(StoreLimits {
        retention: RetentionPolicy {
            retain: Duration::from_hours(2).into(),
            ..RetentionPolicy::default()
        },
        ..StoreLimits::default()
    })
}

fn publish_parents(lease: &StreamLease, local: u32, count: u64) {
    write(lease, initialization(local, 1));
    for id in 0..count {
        write_segment(
            lease,
            local,
            id,
            i64::try_from(id).expect("fixture id fits i64") * 6,
        );
    }
}

fn playlist_version(playlist: &str) -> Option<u8> {
    playlist.lines().find_map(|line| {
        line.strip_prefix("#EXT-X-VERSION:")
            .and_then(|value| value.parse().ok())
    })
}

fn playlist_media_sequence(playlist: &str) -> Option<u64> {
    playlist.lines().find_map(|line| {
        line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")
            .and_then(|value| value.parse().ok())
    })
}

fn skipped_segment_count(playlist: &str) -> Option<u64> {
    playlist.lines().find_map(|line| {
        line.strip_prefix("#EXT-X-SKIP:")
            .and_then(|rest| {
                rest.split(',')
                    .find_map(|attribute| attribute.strip_prefix("SKIPPED-SEGMENTS="))
            })
            .and_then(|value| value.parse().ok())
    })
}

fn media_uris(playlist: &str) -> Vec<&str> {
    playlist
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect()
}

#[test]
fn a_short_window_still_advertises_skip_and_serves_a_noop_delta()
-> Result<(), Box<dyn std::error::Error>> {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let full = render(&lease, 0, &policy())?;
    let delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;

    assert!(full.contains("CAN-SKIP-UNTIL=36"));
    assert!(!full.contains("#EXT-X-SKIP"));
    assert_eq!(
        delta, full,
        "a window shorter than the skip boundary is a full playlist with no \
         EXT-X-SKIP, which is what a client that asked to skip still receives"
    );
    Ok(())
}

#[test]
fn a_delta_keeps_media_sequence_skips_parents_and_reemits_the_map()
-> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let lease = lease(&store, vec![video(0)]);
    publish_parents(&lease, 0, 10);

    let full = render(&lease, 0, &policy())?;
    let delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;

    assert_eq!(playlist_media_sequence(&full), Some(0));
    assert_eq!(
        playlist_media_sequence(&delta),
        playlist_media_sequence(&full),
        "MEDIA-SEQUENCE is the window head, including skipped parents"
    );
    assert_eq!(
        skipped_segment_count(&delta),
        Some(3),
        "ten 6s parents, skip 36s from the last end, equal-to-boundary stays: {delta}"
    );
    assert_eq!(playlist_version(&delta), Some(9));
    assert!(
        !delta.contains("CAN-SKIP-DATERANGES"),
        "an explicit NO is not a spec value: {delta}"
    );

    let skipped = usize::try_from(skipped_segment_count(&delta).expect("delta skips"))?;
    assert_eq!(&media_uris(&delta)[..], &media_uris(&full)[skipped..]);
    assert!(
        !delta.contains("segment/1.m4s"),
        "skipped parents and their parts are omitted: {delta}"
    );
    assert!(
        delta.contains("segment/4.m4s"),
        "the first survivor remains: {delta}"
    );

    let after_skip = delta
        .split_once("#EXT-X-SKIP:")
        .ok_or_else(|| std::io::Error::other("delta contains EXT-X-SKIP"))?
        .1;
    assert!(
        after_skip.contains("#EXT-X-MAP:URI=\"init/1.mp4\""),
        "re-emit the MAP in effect on the first unskipped parent: {delta}"
    );
    assert_eq!(
        delta.matches("#EXT-X-PROGRAM-DATE-TIME").count(),
        0,
        "the first survivor is not the playlist head, so PDT stays where the \
         full playlist put it — on the skipped first parent: {delta}"
    );
    assert_eq!(full.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 1);
    let dense = PlaylistPolicy {
        program_date_time: ProgramDateTimePolicy::EverySegment,
        ..policy()
    };
    let dense_full = render(&lease, 0, &dense)?;
    let dense_delta = render_delta(&lease, 0, &dense, PlaylistDelta::Skip)?;
    let dense_skipped = usize::try_from(skipped_segment_count(&dense_delta).expect("skips"))?;
    assert_eq!(
        dense_delta.matches("#EXT-X-PROGRAM-DATE-TIME").count(),
        dense_full.matches("#EXT-X-PROGRAM-DATE-TIME").count() - dense_skipped,
        "PDT on remaining parents matches the full playlist's tags"
    );
    assert!(
        !delta.contains("#EXT-X-DISCONTINUITY\n"),
        "discontinuity on the first survivor only if that parent had it"
    );
    Ok(())
}

#[test]
fn skipped_segments_counts_parents_not_parts() -> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let lease = lease(&store, vec![video(0)]);
    publish_parents(&lease, 0, 10);
    write(&lease, chunk(0, 10, 0, 60));

    let delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;

    assert_eq!(skipped_segment_count(&delta), Some(3));
    assert!(
        delta.contains("#EXT-X-PART:DURATION=1,URI=\"part/61.m4s\",INDEPENDENT=YES\n"),
        "open parts always stay: {delta}"
    );
    assert!(
        delta.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/62.m4s\"\n"),
        "preload hints always stay: {delta}"
    );
    Ok(())
}

#[test]
fn a_segment_only_delta_is_version_9() -> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let lease = lease(&store, vec![video(0), subtitle(1)]);
    write(&lease, initialization(1, 1));
    for id in 0..10 {
        write_direct(
            &lease,
            1,
            id,
            i64::try_from(id).expect("fixture id fits i64") * 6,
        );
    }

    let full = render(&lease, 1, &policy())?;
    let delta = render_delta(&lease, 1, &policy(), PlaylistDelta::Skip)?;

    assert_eq!(playlist_version(&full), Some(6));
    assert_eq!(
        playlist_version(&delta),
        Some(9),
        "EXT-X-SKIP requires version 9 even on a segment-only playlist: {delta}"
    );
    assert_eq!(skipped_segment_count(&delta), Some(3));
    Ok(())
}

#[test]
fn a_v2_delta_carries_empty_dateranges_and_is_version_10() -> Result<(), Box<dyn std::error::Error>>
{
    let store = long_window();
    let lease = lease(&store, vec![video(0)]);
    publish_parents(&lease, 0, 10);

    let delta = render_delta(&lease, 0, &policy(), PlaylistDelta::SkipV2)?;

    assert_eq!(playlist_version(&delta), Some(10));
    assert!(
        delta.contains("#EXT-X-SKIP:SKIPPED-SEGMENTS=3,RECENTLY-REMOVED-DATERANGES=\"\"\n"),
        "v2 must carry the attribute even when no date ranges were removed: {delta}"
    );
    Ok(())
}

#[test]
fn a_terminal_playlist_ignores_skip() -> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let lease = lease(&store, vec![video(0)]);
    publish_parents(&lease, 0, 10);
    lease.end();

    let full = render(&lease, 0, &policy())?;
    let delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;

    assert!(full.contains("#EXT-X-ENDLIST"));
    assert_eq!(delta, full);
    assert!(!delta.contains("#EXT-X-SKIP"));
    Ok(())
}

#[test]
fn successive_deltas_name_the_same_tail_as_the_full_playlist()
-> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let lease = lease(&store, vec![video(0)]);
    publish_parents(&lease, 0, 10);

    let first_full = render(&lease, 0, &policy())?;
    let first_delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;
    let first_skipped = usize::try_from(skipped_segment_count(&first_delta).expect("skips"))?;
    assert_eq!(
        &media_uris(&first_delta)[..],
        &media_uris(&first_full)[first_skipped..]
    );

    write_segment(&lease, 0, 10, 60);
    write_segment(&lease, 0, 11, 66);

    let second_full = render(&lease, 0, &policy())?;
    let second_delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;
    let second_skipped = usize::try_from(skipped_segment_count(&second_delta).expect("skips"))?;
    assert_eq!(
        &media_uris(&second_delta)[..],
        &media_uris(&second_full)[second_skipped..],
        "a merge of the skipped prefix from the previous full playlist with \
         this delta's tail is the full playlist at this epoch"
    );
    assert_eq!(
        playlist_media_sequence(&second_full),
        playlist_media_sequence(&second_delta)
    );
    Ok(())
}

#[test]
fn a_skipped_discontinuity_does_not_move_the_sequence_number()
-> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write_segment(&first, 0, 0, 0);

    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    for id in 0..10 {
        write_segment(
            &second,
            0,
            id,
            i64::try_from(id).expect("fixture id fits i64") * 6,
        );
    }

    let full = render(&second, 0, &policy())?;
    let delta = render_delta(&second, 0, &policy(), PlaylistDelta::Skip)?;

    assert!(full.contains("#EXT-X-DISCONTINUITY\n"));
    assert!(
        !full.contains("#EXT-X-DISCONTINUITY-SEQUENCE"),
        "the discontinuity is still in the window"
    );
    assert_eq!(
        playlist_media_sequence(&full),
        playlist_media_sequence(&delta)
    );
    assert!(
        !delta.contains("#EXT-X-DISCONTINUITY-SEQUENCE"),
        "skipping a discontinuity does not count it as departed: {delta}"
    );
    Ok(())
}

#[test]
fn discontinuity_on_the_first_survivor_stays_on_that_parent()
-> Result<(), Box<dyn std::error::Error>> {
    // Eight 6s parents: skip 1, keep 7. The takeover's first segment is the
    // survivor, so its EXT-X-DISCONTINUITY must still be written after SKIP.
    let store = long_window();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write_segment(&first, 0, 0, 0);

    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    for id in 0..7 {
        write_segment(
            &second,
            0,
            id,
            i64::try_from(id).expect("fixture id fits i64") * 6,
        );
    }

    let full = render(&second, 0, &policy())?;
    let delta = render_delta(&second, 0, &policy(), PlaylistDelta::Skip)?;

    assert_eq!(skipped_segment_count(&delta), Some(1), "{delta}");
    assert!(full.contains("#EXT-X-DISCONTINUITY\n"));
    let after_skip = delta
        .split_once("#EXT-X-SKIP:")
        .ok_or_else(|| std::io::Error::other("delta contains EXT-X-SKIP"))?
        .1;
    assert!(
        after_skip.contains("#EXT-X-DISCONTINUITY\n"),
        "the first survivor had a discontinuity in the full playlist: {delta}"
    );
    Ok(())
}

#[test]
fn siblings_are_still_reported_on_a_delta() -> Result<(), Box<dyn std::error::Error>> {
    let store = long_window();
    let lease = lease(&store, vec![video(0), audio(1)]);
    publish_parents(&lease, 0, 10);
    write(&lease, initialization(1, 1));
    write_segment(&lease, 1, 0, 0);

    let delta = render_delta(&lease, 0, &policy(), PlaylistDelta::Skip)?;

    assert!(
        delta.contains("#EXT-X-RENDITION-REPORT:URI=\"../1/audio.m3u8\""),
        "rendition reports are playlist-global and always stay: {delta}"
    );
    Ok(())
}
