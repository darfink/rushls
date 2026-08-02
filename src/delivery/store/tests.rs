use std::{sync::Arc, time::SystemTime};

use crate::{
    domain::{MediaKind, Payload, RenditionId, Timebase, fixtures::TrackBuilder},
    media::fixtures::presentation as validated,
    mux::{
        InitializationSegment, MediaSegmentFormat, PackagedChunk, PackagedMedia,
        PackagedPresentation, PackagedRendition, PackagedSegment, PackagedSegmentCompletion,
        PackagingRenditionId, PackagingSegmentId, RenditionConfig, RenditionKey,
        fixtures::{RenditionBuilder, config, presentation_at},
    },
};

use super::*;

/// A one-tick timebase, so a test can say "six seconds" as `6`.
///
/// Segment and part arithmetic is what these tests are about; making the
/// numbers legible keeps a retention expectation from being an exercise in
/// dividing by ninety thousand.
fn timebase() -> Timebase {
    Timebase::new(nz::u32!(1), nz::u32!(1))
}

fn stream() -> StreamId {
    StreamId::new("live/camera")
}

fn limits() -> StoreLimits {
    StoreLimits {
        maximum_streams: 8,
        idle_retention: Duration::from_secs(30),
        retention: RetentionPolicy::default(),
    }
}

fn store() -> StreamStore {
    StreamStore::new(limits())
}

/// One rendition per `(packaging id, chunked)`, six-second segments.
fn rendition(packaging_rendition_id: u32, chunked: bool) -> PackagedRendition {
    RenditionBuilder::new(packaging_rendition_id, MediaKind::Video)
        .key(&format!("video/{packaging_rendition_id}"))
        .config(config(timebase(), 6, chunked.then_some(1)))
        .build()
}

fn packaged_presentation(configs: &[(u32, bool)]) -> Arc<PackagedPresentation> {
    let input = validated(vec![
        TrackBuilder::new(0, MediaKind::Video)
            .timebase(timebase())
            .build(),
    ]);
    let renditions = configs
        .iter()
        .map(|&(packaging_rendition_id, chunked)| rendition(packaging_rendition_id, chunked))
        .collect();
    Arc::new(presentation_at(SystemTime::UNIX_EPOCH, &input, renditions))
}

fn lease(store: &StreamStore, configs: &[(u32, bool)]) -> StreamLease {
    store
        .lease(stream(), packaged_presentation(configs))
        .expect("test publication fits")
}

/// A single-rendition publication with an explicit key and wall-clock origin.
///
/// Both are what reconnect matching turns on, so they are spelled at the call
/// site rather than patched into a presentation after it was validated.
fn keyed_presentation(
    local_id: u32,
    key: &str,
    chunked: bool,
    time_anchor: SystemTime,
) -> Arc<PackagedPresentation> {
    presentation_with(
        local_id,
        key,
        config(timebase(), 6, chunked.then_some(1)),
        time_anchor,
    )
}

/// The same, with the packaging cadence spelled out.
///
/// Cadence is what the playlist contract is derived from, so a test about
/// contracts has to be able to state it rather than inherit the default.
fn presentation_with(
    local_id: u32,
    key: &str,
    rendition_config: RenditionConfig,
    time_anchor: SystemTime,
) -> Arc<PackagedPresentation> {
    let input = validated(vec![
        TrackBuilder::new(0, MediaKind::Video)
            .timebase(rendition_config.timebase)
            .build(),
    ]);
    let rendition = RenditionBuilder::new(local_id, MediaKind::Video)
        .key(key)
        .config(rendition_config)
        .build();
    Arc::new(presentation_at(time_anchor, &input, vec![rendition]))
}

fn initialization(rendition: u32, byte: u8) -> PackagedMedia {
    PackagedMedia::Initialization(InitializationSegment {
        rendition_id: PackagingRenditionId(rendition),
        version: u64::from(byte),
        payload: Payload::from(vec![byte]),
    })
}

fn chunk(
    rendition: u32,
    segment: u64,
    index: u32,
    start: i64,
    duration: u64,
    bytes: usize,
) -> PackagedMedia {
    PackagedMedia::Chunk(PackagedChunk {
        rendition_id: PackagingRenditionId(rendition),
        packaging_segment_id: PackagingSegmentId(segment),
        chunk_index: index,
        media_start: start,
        duration,
        independent: index == 0,
        payload: Payload::from(vec![index as u8; bytes]),
    })
}

fn completion(rendition: u32, segment: u64, start: i64, duration: u64) -> PackagedMedia {
    PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
        rendition_id: PackagingRenditionId(rendition),
        packaging_segment_id: PackagingSegmentId(segment),
        media_start: start,
        duration,
    })
}

fn direct(rendition: u32, segment: u64, start: i64, duration: u64, bytes: usize) -> PackagedMedia {
    PackagedMedia::Segment(PackagedSegment {
        rendition_id: PackagingRenditionId(rendition),
        packaging_segment_id: PackagingSegmentId(segment),
        media_start: start,
        duration,
        independent: true,
        payload: Payload::from(vec![segment as u8; bytes]),
    })
}

fn write(lease: &StreamLease, media: PackagedMedia) {
    assert_eq!(lease.write(media), Ok(true));
}

/// Publishes one whole six-second segment as the parts that compose it.
///
/// A part may not exceed PART-TARGET, so advancing the playlist by a segment
/// means publishing six one-second parts rather than one six-second part.
fn write_segment(lease: &StreamLease, rendition: u32, segment: u64, start: i64) {
    for index in 0..6 {
        write(
            lease,
            chunk(rendition, segment, index, start + i64::from(index), 1, 1),
        );
    }
    write(lease, completion(rendition, segment, start, 6));
}

fn configure(lease: &StreamLease, rendition: u32, chunked: bool) {
    let catalog = lease.live().snapshot();
    let snapshot = catalog
        .renditions
        .iter()
        .find(|entry| entry.key == RenditionKey::new(format!("video/{rendition}")))
        .expect("rendition was configured by the descriptor");
    assert_eq!(
        snapshot
            .config
            .and_then(|config| config.chunk_target)
            .is_some(),
        chunked
    );
    write(lease, initialization(rendition, 1));
}

#[test]
fn fractional_playlist_duration_policy_controls_the_visible_window() {
    let mut limits = limits();
    limits.retention.minimum_playlist_segments = 0;
    limits.retention.minimum_playlist_duration = TargetDurationMultiple::new(3, nz::u32!(2)).into();
    let store = StreamStore::new(limits);
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);

    for id in 0..3 {
        write(&lease, direct(0, id, id as i64 * 6, 6, 1));
    }

    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert_eq!(snapshot.segments.len(), 2);
    assert_eq!(snapshot.segments[0].msn, Msn(1));
}

#[test]
fn invalid_packaging_sequences_are_rejected_atomically() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    assert_eq!(
        lease.write(chunk(9, 0, 0, 0, 1, 3)),
        Err(StoreWriteError::UnknownPackagingRendition {
            rendition_id: PackagingRenditionId(9)
        })
    );

    assert_eq!(
        lease.write(chunk(0, 0, 0, 0, 1, 3)),
        Err(StoreWriteError::InitializationMissing {
            rendition_id: RenditionId(0)
        })
    );
    write(&lease, initialization(0, 1));
    write(&lease, chunk(0, 0, 0, 0, 1, 3));
    let bytes = lease.live().retained_payload_bytes();
    assert_eq!(
        lease.write(chunk(0, 0, 2, 1, 1, 7)),
        Err(StoreWriteError::UnexpectedChunkIndex {
            rendition_id: RenditionId(0),
            expected: 1,
            found: 2,
        })
    );
    assert_eq!(
        lease.write(completion(0, 1, 0, 1)),
        Err(StoreWriteError::WrongSegmentCompleted {
            rendition_id: RenditionId(0),
            open: PackagingSegmentId(0),
            found: PackagingSegmentId(1),
        })
    );
    assert!(matches!(
        lease.write(direct(0, 1, 0, 6, 4)),
        Err(StoreWriteError::DirectSegmentDuringOpenSegment { .. })
    ));
    assert_eq!(
        lease.write(initialization(0, 2)),
        Err(StoreWriteError::InitializationDuringOpenSegment {
            rendition_id: RenditionId(0)
        })
    );
    assert_eq!(lease.live().retained_payload_bytes(), bytes);
    assert_eq!(
        lease
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .open_segment
            .as_ref()
            .unwrap()
            .parts
            .len(),
        1
    );

    write(&lease, completion(0, 0, 0, 1));
    assert_eq!(
        lease.write(chunk(0, 0, 0, 1, 1, 1)),
        Err(StoreWriteError::NonMonotonicSegmentId {
            rendition_id: RenditionId(0),
            previous: PackagingSegmentId(0),
            found: PackagingSegmentId(0),
        })
    );
    assert_eq!(
        lease.write(completion(0, 1, 1, 1)),
        Err(StoreWriteError::NoOpenSegment {
            rendition_id: RenditionId(0)
        })
    );
}

#[test]
fn rendition_configuration_selects_chunked_or_segment_only_packaging() {
    let store = store();
    let lease = lease(&store, &[(0, false), (1, true)]);
    configure(&lease, 0, false);
    assert_eq!(
        lease.write(chunk(0, 0, 0, 0, 1, 1)),
        Err(StoreWriteError::ChunksDisabled {
            rendition_id: RenditionId(0)
        })
    );

    configure(&lease, 1, true);
    assert_eq!(
        lease.write(direct(1, 0, 0, 6, 1)),
        Err(StoreWriteError::DirectSegmentsDisabled {
            rendition_id: RenditionId(1)
        })
    );
    assert!(
        lease
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .segments
            .is_empty()
    );
    assert!(
        lease
            .live()
            .rendition(RenditionId(1))
            .unwrap()
            .segments
            .is_empty()
    );
}

#[test]
fn completing_a_chunked_segment_reuses_the_original_payloads() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    let original = Payload::from(vec![1, 2, 3, 4]);
    let pointer = original.as_bytes().as_ptr();
    write(
        &lease,
        PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(0),
            packaging_segment_id: PackagingSegmentId(0),
            chunk_index: 0,
            media_start: 0,
            duration: 1,
            independent: true,
            payload: original,
        }),
    );
    // The rest of the segment carries no bytes, so the retained total and the
    // observed bitrate below still describe exactly the one tracked payload.
    for index in 1..6 {
        write(&lease, chunk(0, 0, index, index as i64, 1, 0));
    }
    assert!(
        !lease
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .has_completed_segment(),
        "the first parent segment is still open"
    );
    write(&lease, completion(0, 0, 0, 6));

    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert!(snapshot.has_completed_segment());
    let StoredSegmentKind::Media(SegmentBody::Chunked(parts)) = &snapshot.segments[0].kind else {
        panic!("segment should retain its chunks");
    };
    assert_eq!(parts[0].payload.as_bytes().as_ptr(), pointer);
    assert_eq!(
        snapshot.segments[0].kind,
        StoredSegmentKind::Media(SegmentBody::Chunked(Arc::clone(parts)))
    );
    assert_eq!(lease.live().retained_payload_bytes(), 5);
    assert_eq!(
        snapshot.bitrate,
        RenditionBitrateStatistics {
            peak_bits_per_second: Some(5),
            average_bits_per_second: Some(5),
            observed_segments: 1,
        }
    );
}

#[test]
fn direct_segments_remain_contiguous() {
    let store = store();
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    write(&lease, direct(0, 0, 0, 6, 9));

    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    let segment = &snapshot.segments[0];
    assert!(matches!(
        &segment.kind,
        StoredSegmentKind::Media(SegmentBody::Contiguous(payload)) if payload.len() == 9
    ));
    assert!(snapshot.segments.parts(segment).is_empty());
    assert_eq!(
        lease
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .bitrate
            .average_bits_per_second,
        Some(12)
    );
}

#[test]
fn webvtt_initialization_and_direct_segments_are_retained_as_authored() {
    let input = validated(vec![
        TrackBuilder::new(0, MediaKind::Video)
            .timebase(timebase())
            .build(),
        TrackBuilder::new(1, MediaKind::Subtitle)
            .timebase(timebase())
            .build(),
    ]);
    let video = rendition(0, true);
    let subtitle = RenditionBuilder::for_track(
        1,
        input
            .catalog()
            .get(crate::domain::TrackId(1))
            .expect("subtitle track exists"),
    )
    .key("subtitle/english")
    .config(RenditionConfig {
        timebase: timebase(),
        segment_target: nz::u64!(6),
        maximum_segment_duration: nz::u64!(6),
        chunk_target: None,
        segment_format: MediaSegmentFormat::WebVtt,
    })
    .build();
    let presentation = Arc::new(presentation_at(
        SystemTime::UNIX_EPOCH,
        &input,
        vec![video, subtitle],
    ));
    let store = store();
    let lease = store
        .lease(stream(), presentation)
        .expect("WebVTT publication fits");
    let header = b"WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\n";
    let cue = b"00:00:00.000 --> 00:00:01.000\nHello\n\n";

    write(
        &lease,
        PackagedMedia::Initialization(InitializationSegment {
            rendition_id: PackagingRenditionId(1),
            version: 0,
            payload: Payload::from(header.to_vec()),
        }),
    );
    write(
        &lease,
        PackagedMedia::Segment(PackagedSegment {
            rendition_id: PackagingRenditionId(1),
            packaging_segment_id: PackagingSegmentId(0),
            media_start: 0,
            duration: 1,
            independent: true,
            payload: Payload::from(cue.to_vec()),
        }),
    );

    let snapshot = lease
        .live()
        .rendition(RenditionId(1))
        .expect("subtitle rendition is retained");
    assert_eq!(snapshot.initializations[0].payload.as_bytes(), header);
    assert!(matches!(
        &snapshot.segments[0].kind,
        StoredSegmentKind::Media(SegmentBody::Contiguous(payload))
            if payload.as_bytes() == cue
    ));
    assert_eq!(
        snapshot
            .config
            .expect("configuration is advertised")
            .segment_format,
        MediaSegmentFormat::WebVtt
    );
}

#[test]
fn durable_part_ids_and_cursors_advance_independently() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    assert_eq!(
        lease
            .live()
            .rendition_live_edge(RenditionId(0))
            .unwrap()
            .next_part_id,
        Some(PartId(1))
    );
    write(&lease, chunk(0, 0, 0, 0, 1, 1));
    assert_eq!(
        lease.live().rendition_live_edge(RenditionId(0)).unwrap(),
        RenditionLiveEdge {
            last_segment: None,
            last_part: Some((
                PartCursor {
                    msn: Msn(0),
                    part_index: PartIndex(0)
                },
                PartId(1)
            )),
            next_part_id: Some(PartId(2)),
            ended: false,
            revision: 3,
        }
    );
    write(&lease, completion(0, 0, 0, 1));
    write(&lease, chunk(0, 1, 0, 1, 1, 1));

    let edge = lease.live().rendition_live_edge(RenditionId(0)).unwrap();
    assert_eq!(edge.last_segment, Some((Msn(0), SegmentId(1))));
    assert_eq!(
        edge.last_part,
        Some((
            PartCursor {
                msn: Msn(1),
                part_index: PartIndex(0)
            },
            PartId(2)
        ))
    );
    assert_eq!(edge.next_part_id, Some(PartId(3)));
}

#[test]
fn takeover_consumes_an_observed_open_msn_as_a_gap() {
    let store = store();
    let first = lease(&store, &[(0, true)]);
    configure(&first, 0, true);
    write(&first, chunk(0, 0, 0, 0, 1, 1));

    let second = lease(&store, &[(0, true)]);
    assert_eq!(first.write(chunk(0, 0, 1, 1, 1, 1)), Ok(false));
    configure(&second, 0, true);
    write(&second, chunk(0, 0, 0, 0, 1, 1));

    let snapshot = second.live().rendition(RenditionId(0)).unwrap();
    assert!(matches!(snapshot.segments[0].kind, StoredSegmentKind::Gap));
    assert_eq!(snapshot.segments[0].msn, Msn(0));
    assert!(
        snapshot.segments.parts(&snapshot.segments[0]).is_empty(),
        "a gap beyond the visibility frontier cannot expose the parts it replaced"
    );
    assert!(
        second.live().part(RenditionId(0), PartId(1)).is_some(),
        "the hidden part remains independently fetchable during its grace period"
    );
    let open = snapshot.open_segment.as_ref().unwrap();
    assert_eq!(open.msn, Msn(1));
    assert_eq!(open.parts[0].id, PartId(2));
    assert_eq!(open.parts[0].cursor.part_index, PartIndex(0));
}

#[test]
fn reconnect_matches_exact_keys_even_when_local_ids_change() {
    let store = store();
    let first = store
        .lease(
            stream(),
            keyed_presentation(0, "camera/main", false, SystemTime::UNIX_EPOCH),
        )
        .unwrap();
    write(&first, initialization(0, 1));
    write(&first, direct(0, 0, 0, 6, 1));

    let second_anchor = SystemTime::UNIX_EPOCH + Duration::from_secs(30);
    let second = store
        .lease(
            stream(),
            keyed_presentation(9, "camera/main", false, second_anchor),
        )
        .unwrap();
    write(&second, initialization(9, 2));
    write(&second, direct(9, 0, 0, 6, 1));

    let catalog = second.live().snapshot();
    assert_eq!(catalog.renditions.len(), 1);
    assert_eq!(catalog.renditions[0].rendition_id, RenditionId(0));
    assert_eq!(
        catalog.presentation.as_ref().unwrap().groups[0]
            .renditions
            .as_ref(),
        &[RenditionId(0)]
    );
    assert_eq!(catalog.publication_anchors.len(), 2);
    assert_eq!(catalog.publication_anchors[1].time_anchor, second_anchor);
    let media = catalog.renditions[0].snapshot();
    assert_eq!(media.segments.len(), 2);
    assert_eq!(media.segments[1].msn, Msn(1));
}

#[test]
fn changed_topology_retires_old_renditions_instead_of_fuzzy_matching() {
    let store = store();
    let first = store
        .lease(
            stream(),
            keyed_presentation(0, "camera/main", false, SystemTime::UNIX_EPOCH),
        )
        .unwrap();
    write(&first, initialization(0, 1));
    write(&first, direct(0, 0, 0, 6, 1));

    let second = store
        .lease(
            stream(),
            keyed_presentation(0, "camera/replacement", false, SystemTime::UNIX_EPOCH),
        )
        .unwrap();
    write(&second, initialization(0, 1));

    let catalog = second.live().snapshot();
    assert_eq!(catalog.renditions.len(), 2);
    assert!(!catalog.renditions[0].active);
    assert!(catalog.renditions[0].snapshot().live_edge.ended);
    assert!(catalog.renditions[1].active);
    assert_eq!(catalog.renditions[1].rendition_id, RenditionId(1));
    assert_eq!(
        catalog.presentation.as_ref().unwrap().groups[0]
            .renditions
            .as_ref(),
        &[RenditionId(1)]
    );
    assert!(
        second
            .live()
            .segment(RenditionId(0), SegmentId(1))
            .is_some(),
        "retired playlist resources remain fetchable through normal retention"
    );

    let third = store
        .lease(
            stream(),
            keyed_presentation(5, "camera/main", false, SystemTime::UNIX_EPOCH),
        )
        .unwrap();
    let catalog = third.live().snapshot();
    assert_eq!(
        catalog.presentation.as_ref().unwrap().groups[0]
            .renditions
            .as_ref(),
        &[RenditionId(2)],
        "an ended playlist is not resurrected when its key later returns"
    );
}

#[tokio::test(start_paused = true)]
async fn rendition_watchers_are_isolated_and_bitrate_updates_bump_the_catalog() {
    let store = store();
    let lease = lease(&store, &[(0, true), (1, true)]);
    configure(&lease, 0, true);
    configure(&lease, 1, true);
    let catalog_revision = lease.live().revision();
    let media_catalog_revision = lease.live().snapshot().media_catalog_revision;
    let mut first = lease.live().subscribe_rendition(RenditionId(0)).unwrap();
    let mut sibling = lease.live().subscribe_rendition(RenditionId(1)).unwrap();
    first.borrow_and_update();
    sibling.borrow_and_update();

    write(&lease, chunk(0, 0, 0, 0, 1, 1));
    first.changed().await.unwrap();
    assert!(!sibling.has_changed().unwrap());
    assert_eq!(lease.live().revision(), catalog_revision);

    write(&lease, completion(0, 0, 0, 1));
    assert!(first.has_changed().unwrap());
    assert!(!sibling.has_changed().unwrap());
    assert!(
        lease.live().revision() > catalog_revision,
        "completed-segment bitrate statistics invalidate the multivariant projection"
    );
    let catalog = lease.live().snapshot();
    assert_eq!(
        catalog.media_catalog_revision, media_catalog_revision,
        "bandwidth does not invalidate media playlists that never render it"
    );
    assert_eq!(
        catalog.renditions[0].bandwidth,
        catalog.renditions[0].snapshot().bitrate.advertised(),
        "one catalog revision captures the bitrate values it advertises"
    );
}

#[test]
fn unchanged_advertised_bitrate_does_not_churn_the_catalog() {
    let store = store();
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    write(&lease, direct(0, 0, 0, 6, 6));
    let revision = lease.live().revision();

    write(&lease, direct(0, 1, 6, 6, 6));

    assert_eq!(
        lease.live().revision(),
        revision,
        "observation counts are diagnostic; only advertised rates invalidate the manifest"
    );
}

#[test]
fn request_snapshots_are_cached_until_the_next_committed_change() {
    let store = store();
    let lease = lease(&store, &[(0, true), (1, true)]);
    configure(&lease, 0, true);
    configure(&lease, 1, true);

    let catalog = lease.live().snapshot();
    let same_catalog = lease.live().snapshot();
    assert!(Arc::ptr_eq(&catalog, &same_catalog));
    let sibling = catalog.renditions[1].snapshot();
    let first = catalog.renditions[0].snapshot();
    let same = lease.live().rendition(RenditionId(0)).unwrap();
    assert!(Arc::ptr_eq(&first, &same));

    write(&lease, chunk(0, 0, 0, 0, 1, 1));
    let same_catalog = lease.live().snapshot();
    assert!(
        Arc::ptr_eq(&catalog, &same_catalog),
        "ordinary chunks do not rebuild the stream catalog"
    );
    assert!(
        Arc::ptr_eq(&sibling, &same_catalog.renditions[1].snapshot()),
        "an unrelated rendition does not rebuild its cached snapshot"
    );
    let advanced = same_catalog.renditions[0].snapshot();
    assert!(!Arc::ptr_eq(&first, &advanced));
    assert!(first.open_segment.is_none());
    assert_eq!(
        advanced.open_segment.as_ref().map(|open| open.parts.len()),
        Some(1)
    );
}

#[test]
fn rendition_start_offsets_are_preserved_without_cross_track_rejection() {
    let store = store();
    let lease = lease(&store, &[(0, false), (1, false)]);
    configure(&lease, 0, false);
    configure(&lease, 1, false);

    write(&lease, direct(0, 0, -22, 6, 1));
    write(&lease, direct(1, 0, 0, 6, 1));

    let first = lease.live().rendition(RenditionId(0)).unwrap();
    let second = lease.live().rendition(RenditionId(1)).unwrap();
    assert_eq!(first.segments[0].media_start, -22);
    assert_eq!(second.segments[0].media_start, 0);
}

#[tokio::test(start_paused = true)]
async fn part_tags_and_resources_have_distinct_retention_deadlines() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    write(&lease, chunk(0, 0, 0, 0, 1, 1));
    write(&lease, completion(0, 0, 0, 1));
    let part_id = PartId(1);

    // Six one-second parts per segment: a part may not exceed PART-TARGET, so
    // advancing the live edge by a whole segment means publishing the parts
    // that actually compose one.
    for id in 1..=4 {
        let start = 1 + (id as i64 - 1) * 6;
        for index in 0..6 {
            write(&lease, chunk(0, id, index, start + index as i64, 1, 1));
        }
        write(&lease, completion(0, id, start, 6));
    }
    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert!(
        snapshot.segments.parts(&snapshot.segments[0]).is_empty(),
        "the tag is hidden once it is over three targets behind"
    );
    assert!(lease.live().part(RenditionId(0), part_id).is_some());

    tokio::time::advance(Duration::from_secs(18)).await;
    assert!(lease.live().part(RenditionId(0), part_id).is_none());
}

#[test]
fn part_tag_retention_never_exposes_only_a_parent_segment_suffix() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);

    for id in 0..3 {
        write_segment(&lease, 0, id, id as i64 * 6);
    }

    // At 21 seconds the first parts of segment zero are individually older
    // than the 18-second retention, but its final part is not. All six tags
    // must remain until the parent can disappear as a unit.
    for index in 0..3 {
        write(&lease, chunk(0, 3, index, 18 + i64::from(index), 1, 1));
    }
    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    let parts = snapshot.segments.parts(&snapshot.segments[0]);
    assert_eq!(parts.len(), 6);
    assert_eq!(parts[0].media_start, snapshot.segments[0].media_start);
    assert_eq!(
        parts.iter().map(|part| part.duration).sum::<u64>(),
        snapshot.segments[0].duration
    );

    for index in 3..6 {
        write(&lease, chunk(0, 3, index, 18 + i64::from(index), 1, 1));
    }
    write(&lease, completion(0, 3, 18, 6));
    write(&lease, chunk(0, 4, 0, 24, 1, 1));

    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert!(
        snapshot.segments.parts(&snapshot.segments[0]).is_empty(),
        "all PART tags leave once the final part exceeds retention"
    );
}

#[tokio::test(start_paused = true)]
async fn removed_segments_obey_their_availability_deadline() {
    let store = store();
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    for id in 0..7 {
        write(&lease, direct(0, id, id as i64 * 6, 6, 1));
        if id < 6 {
            tokio::time::advance(Duration::from_secs(6)).await;
        }
    }
    let first = SegmentId(1);
    assert!(lease.live().segment(RenditionId(0), first).is_some());
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(lease.live().segment(RenditionId(0), first).is_none());
    assert_eq!(
        lease
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .segments
            .len(),
        6
    );
}

#[test]
fn live_window_never_falls_below_three_target_durations() {
    let store = store();
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    for id in 0..19 {
        write(&lease, direct(0, id, id as i64, 1, 1));
    }

    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert_eq!(
        snapshot.segments.len(),
        18,
        "the six-segment count is only a floor when segments are shorter \
         than the target duration"
    );
    assert_eq!(snapshot.segments[0].msn, Msn(1));
}

#[tokio::test(start_paused = true)]
async fn initializations_live_until_every_dependent_resource_expires() {
    let store = store();
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    write(&lease, direct(0, 0, 0, 6, 1));
    write(&lease, initialization(0, 2));
    for id in 1..7 {
        write(&lease, direct(0, id, id as i64 * 6, 6, 1));
    }

    assert_eq!(
        lease
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .initializations
            .len(),
        2
    );
    tokio::time::advance(Duration::from_secs(42)).await;
    store.maintain();
    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert_eq!(snapshot.initializations.len(), 1);
    assert_eq!(snapshot.initializations[0].version, 2);
}

#[test]
fn payload_capacity_failure_does_not_mutate_the_open_segment() {
    let mut limits = limits();
    limits.retention.maximum_payload_bytes = 4;
    let store = StreamStore::new(limits);
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    write(&lease, chunk(0, 0, 0, 0, 1, 3));
    assert_eq!(lease.live().retained_payload_bytes(), 4);

    assert_eq!(
        lease.write(chunk(0, 0, 1, 1, 1, 1)),
        Err(StoreWriteError::PayloadCapacityExceeded {
            maximum: 4,
            additional: 1,
        })
    );
    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    let open = snapshot.open_segment.as_ref().unwrap();
    assert_eq!(open.parts.len(), 1);
    assert_eq!(lease.live().retained_payload_bytes(), 4);
}

#[test]
fn object_capacity_failure_is_atomic_even_for_empty_payloads() {
    let mut limits = limits();
    limits.retention.maximum_parts = 1;
    let store = StreamStore::new(limits);
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    write(&lease, chunk(0, 0, 0, 0, 1, 0));

    assert_eq!(
        lease.write(chunk(0, 0, 1, 1, 1, 0)),
        Err(StoreWriteError::PartCapacityExceeded { maximum: 1 })
    );
    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert_eq!(snapshot.open_segment.as_ref().unwrap().parts.len(), 1);
    assert_eq!(snapshot.live_edge.next_part_id, Some(PartId(2)));
}

#[test]
fn segment_count_capacity_failure_is_atomic() {
    let mut limits = limits();
    limits.retention.maximum_segments = 6;
    let store = StreamStore::new(limits);
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    for id in 0..6 {
        write(&lease, direct(0, id, id as i64 * 6, 6, 0));
    }

    assert_eq!(
        lease.write(direct(0, 6, 36, 6, 0)),
        Err(StoreWriteError::SegmentCapacityExceeded { maximum: 6 })
    );
    let snapshot = lease.live().rendition(RenditionId(0)).unwrap();
    assert_eq!(snapshot.segments.len(), 6);
    assert_eq!(
        snapshot.live_edge.last_segment,
        Some((Msn(5), SegmentId(6)))
    );
}

#[test]
fn ending_and_releasing_a_rendition_keeps_it_terminal() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    write(&lease, chunk(0, 0, 0, 0, 1, 1));
    let live = Arc::clone(lease.live());
    assert!(lease.end());
    drop(lease);

    let edge = live.rendition_live_edge(RenditionId(0)).unwrap();
    assert!(edge.ended);
    assert_eq!(edge.next_part_id, None);
    assert_eq!(edge.last_segment, Some((Msn(0), SegmentId(1))));
    assert_eq!(
        live.rendition(RenditionId(0))
            .unwrap()
            .bitrate
            .observed_segments,
        0,
        "the synthesized gap is not a bitrate observation"
    );
}

#[test]
fn peak_is_monotonic_and_average_uses_the_latest_media_hour() {
    let store = store();
    let lease = lease(&store, &[(0, false)]);
    configure(&lease, 0, false);
    for id in 0..600 {
        write(&lease, direct(0, id, id as i64 * 6, 6, 12));
    }
    for id in 600..1_200 {
        write(&lease, direct(0, id, id as i64 * 6, 6, 6));
    }

    let stats = lease.live().rendition(RenditionId(0)).unwrap().bitrate;
    assert_eq!(stats.peak_bits_per_second, Some(16));
    assert_eq!(stats.average_bits_per_second, Some(8));
    assert_eq!(stats.observed_segments, 1_200);
}

#[test]
fn bitrate_peak_does_not_span_a_publication_discontinuity() {
    let store = store();
    let first = lease(&store, &[(0, false)]);
    configure(&first, 0, false);
    write(&first, direct(0, 0, 0, 2, 100));

    let second = lease(&store, &[(0, false)]);
    configure(&second, 0, false);
    write(&second, direct(0, 0, 0, 2, 100));
    assert_eq!(
        second
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .bitrate
            .peak_bits_per_second,
        None
    );

    write(&second, direct(0, 1, 2, 2, 100));
    assert_eq!(
        second
            .live()
            .rendition(RenditionId(0))
            .unwrap()
            .bitrate
            .peak_bits_per_second,
        Some(400)
    );
}

#[tokio::test]
async fn lock_free_lookups_coexist_with_takeover_and_retirement_checks() {
    let store = Arc::new(store());
    let held = lease(&store, &[(0, true)]);
    let mut readers = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        readers.push(tokio::spawn(async move {
            for _ in 0..1_000 {
                assert!(store.get(&stream()).is_some());
            }
        }));
    }
    for _ in 0..32 {
        drop(lease(&store, &[(0, true)]));
        store.maintain();
    }
    for reader in readers {
        reader.await.unwrap();
    }
    assert!(store.get(&stream()).is_some());
    drop(held);
}

#[test]
fn a_takeover_marks_the_open_segment_before_any_of_its_parts_is_tagged() {
    let store = store();
    let first = lease(&store, &[(0, true)]);
    configure(&first, 0, true);
    write(&first, chunk(0, 0, 0, 0, 1, 1));

    let second = lease(&store, &[(0, true)]);
    configure(&second, 0, true);
    write(&second, chunk(0, 0, 0, 0, 1, 1));

    let snapshot = second.live().rendition(RenditionId(0)).unwrap();
    let open = snapshot
        .open_segment
        .as_ref()
        .expect("the successor opened a segment");
    assert!(
        open.discontinuity_before,
        "a projection has to place the discontinuity before this segment's first \
         part tag, which is published long before the segment completes"
    );
    assert!(
        !snapshot.segments[0].discontinuity_before,
        "the first parent a rendition ever creates follows nothing"
    );
    assert_eq!(
        (snapshot.media_sequence, snapshot.discontinuity_sequence),
        (0, 0),
        "a tag still inside the window has not been removed from it"
    );
}

#[test]
fn an_evicted_discontinuity_becomes_a_sequence_number_rather_than_nothing() {
    let store = store();
    let first = lease(&store, &[(0, true)]);
    configure(&first, 0, true);
    write(&first, chunk(0, 0, 0, 0, 1, 1));

    // The successor publishes enough segments to push its own first one — the
    // segment carrying the discontinuity — out of the visible window.
    let second = lease(&store, &[(0, true)]);
    configure(&second, 0, true);
    for id in 0..7 {
        write_segment(&second, 0, id, id as i64 * 6);
    }

    let snapshot = second.live().rendition(RenditionId(0)).unwrap();
    assert_eq!(snapshot.segments.len(), 6);
    assert_eq!(
        snapshot.media_sequence, 2,
        "the window head is the gap and the discontinuous segment behind it"
    );
    assert_eq!(
        snapshot.discontinuity_sequence, 1,
        "the splice is still described once its tag is gone; a viewer joining \
         now must not read the window as continuous with what preceded it"
    );
    assert!(
        snapshot
            .segments
            .iter()
            .all(|segment| !segment.discontinuity_before),
        "nothing left in the window is itself a splice"
    );
}

#[test]
fn a_reconnect_that_changes_cadence_gets_a_new_playlist_rather_than_new_terms() {
    let store = store();
    let first = store
        .lease(
            stream(),
            presentation_with(
                0,
                "camera/main",
                config(timebase(), 6, None),
                SystemTime::UNIX_EPOCH,
            ),
        )
        .unwrap();
    write(&first, initialization(0, 1));
    write(&first, direct(0, 0, 0, 6, 1));

    let _second = store
        .lease(
            stream(),
            presentation_with(
                0,
                "camera/main",
                config(timebase(), 10, None),
                SystemTime::UNIX_EPOCH,
            ),
        )
        .unwrap();

    let catalog = store.get(&stream()).unwrap().snapshot();
    assert_eq!(
        catalog.renditions.len(),
        2,
        "the same key at a different cadence is a different playlist, because \
         EXT-X-TARGETDURATION cannot change under viewers already reading one"
    );
    assert_eq!(
        (
            catalog.renditions[0].contract.target_duration,
            catalog.renditions[1].contract.target_duration
        ),
        (nz::u64!(6), nz::u64!(10))
    );
    assert!(!catalog.renditions[0].active && catalog.renditions[1].active);
    assert!(
        catalog.renditions[0].snapshot().live_edge.ended,
        "the playlist that cannot continue ends cleanly instead of stalling"
    );
}

#[test]
fn media_breaking_the_advertised_target_is_refused_without_disturbing_the_playlist() {
    let store = store();
    let lease = lease(&store, &[(0, true)]);
    configure(&lease, 0, true);
    for index in 0..6 {
        write(&lease, chunk(0, 0, index, i64::from(index), 1, 1));
    }
    let published = lease.live().rendition(RenditionId(0)).unwrap();

    assert!(matches!(
        lease.write(chunk(0, 0, 6, 6, 1, 1)),
        Err(StoreWriteError::SegmentTooLong { .. })
    ));
    assert!(
        Arc::ptr_eq(&published, &lease.live().rendition(RenditionId(0)).unwrap()),
        "a refused write leaves the request-facing snapshot exactly as it was"
    );
    assert_eq!(
        lease.write(completion(0, 0, 0, 6)),
        Ok(true),
        "the segment the publisher actually produced is still accepted"
    );
}

#[test]
fn a_part_is_held_to_its_target_at_both_ends() {
    let store = store();
    // Milliseconds, so a part can be a fraction of its target: six-second
    // segments cut into one-second parts.
    let millisecond = Timebase::new(nz::u32!(1), nz::u32!(1_000));
    let lease = store
        .lease(
            stream(),
            presentation_with(
                0,
                "camera/main",
                config(millisecond, 6_000, Some(1_000)),
                SystemTime::UNIX_EPOCH,
            ),
        )
        .unwrap();
    write(&lease, initialization(0, 1));

    assert!(
        matches!(
            lease.write(chunk(0, 0, 0, 0, 1_001, 1)),
            Err(StoreWriteError::PartTooLong { .. })
        ),
        "a part longer than PART-TARGET is refused outright"
    );

    write(&lease, chunk(0, 0, 0, 0, 500, 1));
    assert!(
        matches!(
            lease.write(chunk(0, 0, 1, 500, 1_000, 1)),
            Err(StoreWriteError::PartTooShort {
                minimum,
                ..
            }) if minimum == Duration::from_millis(850)
        ),
        "a short part is legal only as the last of its segment, so its \
         successor's arrival is what makes it invalid"
    );
    assert_eq!(
        lease.write(completion(0, 0, 0, 500)),
        Ok(true),
        "closing the segment leaves the short part final, and legal"
    );
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_does_not_announce_a_stream_viewers_never_lost() {
    let store = StreamStore::new(limits());
    let first = lease(&store, &[(0, true)]);

    // Leasing alone is not availability: nothing is fetchable until a
    // presentation resolves, which is what a viewer actually needs.
    assert_eq!(store.maintain(), Maintenance::default());
    first
        .write_encoded(initialization(0, 1), None)
        .expect("the header is stored");
    first
        .write_encoded(chunk(0, 0, 0, 0, 1, 16), None)
        .expect("the first part is stored");

    assert!(
        first.live().claim_availability(),
        "the first playable commit can claim the transition"
    );
    assert!(
        !first.live().claim_availability(),
        "a latched announcement is not repeated on later media"
    );
    assert_eq!(
        store.maintain(),
        Maintenance::default(),
        "maintenance is responsible only for time-based retirement"
    );

    // The publisher goes away and comes back inside the reconnect window.
    drop(first);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(store.maintain(), Maintenance::default());
    let second = lease(&store, &[(0, true)]);
    second
        .write_encoded(initialization(0, 2), None)
        .expect("the header is stored");
    second
        .write_encoded(chunk(0, 0, 0, 0, 1, 16), None)
        .expect("the second publication is stored");

    assert_eq!(
        store.maintain(),
        Maintenance::default(),
        "viewers kept playing across the gap, so nothing about the stream \
         changed; only the publisher did"
    );

    // Nobody comes back this time.
    drop(second);
    tokio::time::advance(limits().idle_retention + Duration::from_secs(1)).await;

    assert_eq!(
        store.maintain().retired,
        vec![stream()],
        "the reconnect window closing is the stream's own end, and the only \
         point at which viewers begin getting 404s"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_never_served_anything_never_became_unavailable() {
    let store = StreamStore::new(limits());
    let lease = lease(&store, &[(0, true)]);

    drop(lease);
    tokio::time::advance(limits().idle_retention + Duration::from_secs(1)).await;

    assert_eq!(
        store.maintain(),
        Maintenance::default(),
        "a stream viewers could never reach cannot stop being reachable"
    );
    assert_eq!(store.len(), 0, "it is still retired, just not announced");
}
