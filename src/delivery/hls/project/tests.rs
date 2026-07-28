//! Projection tests, driven through a real store rather than hand-built
//! snapshots.

use std::sync::Arc;

use crate::{
    delivery::hls::{
        RenditionSnapshot, StreamLease, StreamSnapshot, StreamStore,
        fixtures::{
            audio, chunk, initialization, lease, stream_id, subtitle, video, write, write_direct,
            write_segment,
        },
        project::{
            DeliveryTimingPolicy, PlaylistPolicy, ProgramDateTimePolicy, media::media_playlist,
            multivariant::multivariant_playlist, presentation_server_control,
        },
        uri::{PlaylistUris, UriBase},
    },
    domain::RenditionId,
};

fn policy() -> PlaylistPolicy {
    PlaylistPolicy::default()
}

/// The default: every name relative to the playlist that emits it.
fn uris() -> PlaylistUris {
    UriBase::default().uris(&stream_id())
}

fn snapshots(lease: &StreamLease, rendition: u32) -> (Arc<StreamSnapshot>, Arc<RenditionSnapshot>) {
    let stream = lease.live().snapshot();
    let media = lease
        .live()
        .rendition(RenditionId(rendition))
        .expect("the rendition is in the catalog");
    (stream, media)
}

fn render(lease: &StreamLease, rendition: u32, policy: &PlaylistPolicy) -> String {
    let (stream, media) = snapshots(lease, rendition);
    let control = presentation_server_control(&stream, DeliveryTimingPolicy::default());
    media_playlist(&stream, &media, control, policy, &uris()).expect("the playlist projects")
}

#[test]
fn a_live_playlist_states_its_terms_before_any_media() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let rendered = render(&lease, 0, &policy());
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
            "#EXT-X-SERVER-CONTROL:HOLD-BACK=18,PART-HOLD-BACK=3,CAN-BLOCK-RELOAD=YES",
            "#EXT-X-PART-INF:PART-TARGET=1",
            "#EXT-X-MEDIA-SEQUENCE:0",
        ],
        "the target, the part target, and the hold-backs all come from the \
         locked plan, so they are known before a single segment exists"
    );
}

#[test]
fn a_completed_segment_carries_its_map_and_its_parts() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let rendered = render(&lease, 0, &policy());

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
}

#[test]
fn an_open_segment_is_described_before_its_first_part_is_offered() {
    let store = StreamStore::default();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write_segment(&first, 0, 0, 0);

    // A takeover: the successor's open segment is a splice, and its parts are
    // fetchable long before the segment it belongs to completes.
    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    write(&second, chunk(0, 0, 0, 0));

    let rendered = render(&second, 0, &policy());
    let leading_up_to_the_part = rendered
        .split_once("#EXT-X-PART:DURATION=1,URI=\"part/7.m4s\"")
        .expect("the successor's first part is tagged")
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
}

#[test]
fn a_departed_discontinuity_survives_as_a_sequence_number() {
    let store = StreamStore::default();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write(&first, chunk(0, 0, 0, 0));

    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    for id in 0..7 {
        write_segment(&second, 0, id, id as i64 * 6);
    }

    let rendered = render(&second, 0, &policy());

    assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
    assert!(rendered.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
    assert!(
        !rendered.contains("#EXT-X-DISCONTINUITY\n"),
        "the tag left with its segment; the sequence number is what remains"
    );
}

#[test]
fn a_preload_hint_names_the_part_that_does_not_exist_yet() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write(&lease, chunk(0, 0, 0, 0));

    let rendered = render(&lease, 0, &policy());

    assert!(rendered.contains("#EXT-X-PART:DURATION=1,URI=\"part/1.m4s\",INDEPENDENT=YES\n"));
    assert!(rendered.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/2.m4s\"\n"));
    assert!(!rendered.contains("#EXT-X-ENDLIST"));
}

#[test]
fn ending_a_publication_closes_every_playlist_and_withdraws_the_hint() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    lease.end();

    let rendered = render(&lease, 0, &policy());

    assert!(rendered.ends_with("#EXT-X-ENDLIST\n"));
    assert!(
        !rendered.contains("PRELOAD-HINT"),
        "hinting media after an ENDLIST would park a client on a fetch that \
         can never be satisfied"
    );
}

#[test]
fn siblings_are_reported_so_a_switching_client_knows_where_to_resume() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }
    write(&lease, chunk(1, 1, 0, 6));

    let rendered = render(&lease, 0, &policy());

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
}

#[test]
fn a_configured_base_makes_every_name_absolute() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }

    let (stream, media) = snapshots(&lease, 0);
    let control = presentation_server_control(&stream, DeliveryTimingPolicy::default());
    let uris = UriBase::new("https://cdn.example.com/hls").uris(&stream_id());
    let rendered =
        media_playlist(&stream, &media, control, &policy(), &uris).expect("the playlist projects");
    let multivariant = multivariant_playlist(&stream, &policy(), &uris)
        .expect("the presentation projects")
        .expect("an attached publication has a topology");

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
}

#[test]
fn a_webvtt_playlist_names_vtt_resources_and_advertises_no_parts() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), subtitle(1)]);
    write(&lease, initialization(1, 1));
    write_direct(&lease, 1, 0, 0);

    let rendered = render(&lease, 1, &policy());

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
}

#[test]
fn a_gap_is_tagged_rather_than_omitted() {
    let store = StreamStore::default();
    let first = lease(&store, vec![video(0)]);
    write(&first, initialization(0, 1));
    write(&first, chunk(0, 0, 0, 0));

    // Takeover strands the open segment, which the store resolves as a gap so
    // the MSN a client already saw does not simply vanish.
    let second = lease(&store, vec![video(0)]);
    write(&second, initialization(0, 2));
    write(&second, chunk(0, 0, 0, 0));

    let rendered = render(&second, 0, &policy());

    assert!(rendered.contains("#EXT-X-GAP\n#EXTINF:6,\nsegment/1.m4s\n"));
}

#[test]
fn program_date_time_can_be_restated_on_every_segment() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    write_segment(&lease, 0, 1, 6);

    let sparse = render(&lease, 0, &policy());
    let dense = render(
        &lease,
        0,
        &PlaylistPolicy {
            program_date_time: ProgramDateTimePolicy::EverySegment,
            ..policy()
        },
    );

    assert_eq!(sparse.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 1);
    assert_eq!(dense.matches("#EXT-X-PROGRAM-DATE-TIME").count(), 2);
    assert!(dense.contains("#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:26Z\n"));
}

#[test]
fn a_variant_advertises_what_playing_it_actually_costs() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1), subtitle(2)]);
    for local in [0, 1] {
        write(&lease, initialization(local, 1));
        write_segment(&lease, local, 0, 0);
    }
    write(&lease, initialization(2, 1));
    write_direct(&lease, 2, 0, 0);

    let stream = lease.live().snapshot();
    let rendered = multivariant_playlist(&stream, &policy(), &uris())
        .expect("the presentation projects")
        .expect("an attached publication has a topology");

    let rate = |index: usize| {
        stream.renditions[index]
            .bandwidth
            .peak_bits_per_second
            .expect("a completed segment has been measured")
    };

    assert!(
        rendered.contains(&format!("BANDWIDTH={}", rate(0) + rate(1) + rate(2))),
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
}

#[test]
fn an_unmeasured_presentation_is_servable_immediately() {
    let store = StreamStore::default();
    let lease = lease(&store, vec![video(0), audio(1)]);

    let stream = lease.live().snapshot();
    let rendered = multivariant_playlist(&stream, &policy(), &uris())
        .expect("the presentation projects")
        .expect("a topology exists as soon as a publisher attaches");

    assert!(
        rendered.contains(&format!("BANDWIDTH={}", 6_000_000 * 2)),
        "with nothing measured or declared, both renditions fall back to the \
         configured assumption so startup is not delayed by a segment: {rendered}"
    );
    assert!(
        !rendered.contains("AVERAGE-BANDWIDTH"),
        "an average nobody has measured would describe nothing"
    );
}

#[test]
fn an_optimistic_declaration_is_not_lowered_by_a_smaller_measurement() {
    let store = StreamStore::default();
    let mut declared = video(0);
    declared.declared_bandwidth = Some(9_000_000);
    let lease = lease(&store, vec![declared]);

    let before = multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())
        .expect("projects")
        .expect("has a topology");
    assert!(before.contains("BANDWIDTH=9000000"));

    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    let stream = lease.live().snapshot();
    let measured = stream.renditions[0]
        .bandwidth
        .peak_bits_per_second
        .expect("a completed segment has been measured");

    let after = multivariant_playlist(&stream, &policy(), &uris())
        .expect("projects")
        .expect("has a topology");
    assert!(
        after.contains("BANDWIDTH=9000000"),
        "under-advertising is what makes a client pick a variant it cannot \
         sustain, so the larger of the two wins: measured {measured}"
    );
}

#[test]
fn a_stream_nobody_has_published_to_has_no_presentation() {
    let store = StreamStore::default();
    let lease = store
        .lease_without_presentation(stream_id())
        .expect("the store has room");

    assert!(
        multivariant_playlist(&lease.live().snapshot(), &policy(), &uris())
            .expect("projects")
            .is_none()
    );
}
