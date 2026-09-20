//! Publications for delivery tests, built through the real store.
//!
//! Projection and request handling are both mostly questions about what the
//! store decided — which segment carries a discontinuity, which parts are still
//! tagged, where the live edge is — so these drive an actual [`StreamStore`]
//! rather than assembling snapshots by hand. A hand-built snapshot would test
//! the layer above against a model of the store instead of the store.

use std::{sync::Arc, time::SystemTime};

use crate::{
    domain::{MediaKind, Payload, StreamId, Timebase, TrackId, fixtures::TrackBuilder},
    media::fixtures::presentation as validated,
    mux::{
        InitializationSegment, MediaSegmentFormat, PackagedChunk, PackagedMedia,
        PackagedPresentation, PackagedRendition, PackagedSegment, PackagedSegmentCompletion,
        PackagingRenditionId, PackagingSegmentId, RenditionConfig,
        fixtures::{RenditionBuilder, config, presentation_at},
    },
};

use super::{StreamLease, StreamStore};

/// Bytes in each published part, so a bitrate assertion has something to weigh.
pub const PART_BYTES: usize = 1_024;

/// One tick is one second, so a playlist's numbers read as the durations they
/// are rather than as multiples of ninety thousand.
pub fn timebase() -> Timebase {
    Timebase::new(nz::u32!(1), nz::u32!(1))
}

/// A fixed wall-clock origin, so PROGRAM-DATE-TIME is assertable verbatim.
pub fn anchor() -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
}

pub fn stream_id() -> StreamId {
    StreamId::new("live/camera")
}

/// A video rendition, chunked into one-second parts within six-second segments.
pub fn video(local: u32) -> PackagedRendition {
    video_with_cadence(local, 6, 1)
}

/// The same, with the cadence spelled out.
///
/// Useful where the *derived* values matter rather than the media: a blocking
/// reload's deadline is three target durations, so a test that has to watch one
/// expire wants a short target rather than eighteen seconds of waiting.
pub fn video_with_cadence(local: u32, segment_ticks: u64, chunk_ticks: u64) -> PackagedRendition {
    RenditionBuilder::new(local, MediaKind::Video)
        .key(&format!("video/{local}"))
        .config(config(timebase(), segment_ticks, Some(chunk_ticks)))
        .build()
}

pub fn audio(local: u32) -> PackagedRendition {
    RenditionBuilder::new(local, MediaKind::Audio)
        .key(&format!("audio/{local}"))
        .config(config(timebase(), 6, Some(1)))
        .build()
}

/// A segment-only WebVTT rendition, as the pass-through muxer produces.
pub fn subtitle(local: u32) -> PackagedRendition {
    RenditionBuilder::new(local, MediaKind::Subtitle)
        .key(&format!("subtitle/{local}"))
        .config(RenditionConfig {
            timebase: timebase(),
            segment_target: nz::u64!(6),
            maximum_segment_duration: nz::u64!(6),
            chunk_target: None,
            segment_format: MediaSegmentFormat::WebVtt,
        })
        .build()
}

/// A WebVTT rendition cut into parts, as a low-latency part plan produces.
///
/// Segment-only subtitles remain reachable — a plan whose part target equals
/// its segment target has no grid to subdivide — so both shapes are fixtures
/// rather than one replacing the other.
pub fn subtitle_with_parts(local: u32) -> PackagedRendition {
    RenditionBuilder::new(local, MediaKind::Subtitle)
        .key(&format!("subtitle/{local}"))
        .config(RenditionConfig {
            timebase: timebase(),
            segment_target: nz::u64!(6),
            maximum_segment_duration: nz::u64!(6),
            chunk_target: Some(nz::u64!(1)),
            segment_format: MediaSegmentFormat::WebVtt,
        })
        .build()
}

/// Wraps renditions in a validated presentation, one source track each.
pub fn presentation(renditions: Vec<PackagedRendition>) -> Arc<PackagedPresentation> {
    let tracks: Vec<_> = renditions
        .iter()
        .enumerate()
        .map(|(index, rendition)| {
            TrackBuilder::new(
                u32::try_from(index).expect("fixture track count fits u32"),
                rendition.media.kind(),
            )
            .timebase(timebase())
            .build()
        })
        .collect();
    let input = validated(tracks);
    let renditions = renditions
        .into_iter()
        .enumerate()
        .map(|(index, mut rendition)| {
            rendition.source_tracks = Arc::from([TrackId(
                u32::try_from(index).expect("fixture track count fits u32"),
            )]);
            rendition
        })
        .collect();
    Arc::new(presentation_at(anchor(), &input, renditions))
}

pub fn lease(store: &StreamStore, renditions: Vec<PackagedRendition>) -> StreamLease {
    store
        .lease(stream_id(), &presentation(renditions))
        .expect("the test publication fits")
}

pub fn initialization(local: u32, version: u8) -> PackagedMedia {
    PackagedMedia::Initialization(InitializationSegment {
        rendition_id: PackagingRenditionId(local),
        version: u64::from(version),
        payload: Payload::from(vec![version]),
    })
}

pub fn chunk(local: u32, segment: u64, index: u32, start: i64) -> PackagedMedia {
    PackagedMedia::Chunk(PackagedChunk {
        rendition_id: PackagingRenditionId(local),
        packaging_segment_id: PackagingSegmentId(segment),
        chunk_index: index,
        media_start: start,
        duration: 1,
        independent: index == 0,
        payload: Payload::from(vec![
            u8::try_from(index)
                .expect("fixture chunk index fits u8");
            PART_BYTES
        ]),
    })
}

pub fn write(lease: &StreamLease, media: PackagedMedia) {
    assert_eq!(lease.write(media), Ok(true));
}

/// Publishes one whole six-second segment as its six one-second parts.
///
/// A part may not exceed PART-TARGET, so this is what publishing a segment
/// actually looks like rather than one six-second chunk.
pub fn write_segment(lease: &StreamLease, local: u32, segment: u64, start: i64) {
    for index in 0..6 {
        write(
            lease,
            chunk(local, segment, index, start + i64::from(index)),
        );
    }
    write(
        lease,
        PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
            rendition_id: PackagingRenditionId(local),
            packaging_segment_id: PackagingSegmentId(segment),
            media_start: start,
            duration: 6,
        }),
    );
}

/// Publishes one whole segment in a single object, as a segment-only rendition
/// does.
pub fn write_direct(lease: &StreamLease, local: u32, segment: u64, start: i64) {
    write(
        lease,
        PackagedMedia::Segment(PackagedSegment {
            rendition_id: PackagingRenditionId(local),
            packaging_segment_id: PackagingSegmentId(segment),
            media_start: start,
            duration: 6,
            independent: true,
            payload: Payload::from(vec![7; 256]),
        }),
    );
}

/// A real fragment header with an IDR followed by a dependent AVC sample.
/// At two ticks per second the two samples make one one-second part.
pub fn cmaf_fragment(sync: bool) -> Result<Payload, Box<dyn std::error::Error>> {
    cmaf_fragment_at(sync, 0, 0)
}

/// A fragment with explicit decode and composition timing for index tests.
pub fn cmaf_fragment_at(
    sync: bool,
    decode: i64,
    composition: i64,
) -> Result<Payload, Box<dyn std::error::Error>> {
    use crate::mux::fixtures::{H264_IDR, H264_P};
    let samples = [
        transmux::Sample::new(
            bytes::Bytes::from_static(H264_IDR),
            Some(decode),
            Some(decode + composition),
            Some(1),
            sync,
        ),
        transmux::Sample::new(
            bytes::Bytes::from_static(H264_P),
            Some(decode + 1),
            Some(decode + composition + 1),
            Some(1),
            false,
        ),
    ];
    Ok(Payload::from_bytes(transmux::build_media_segment(
        1,
        &[transmux::FragmentTrackData::new(
            1,
            u64::try_from(decode)?,
            &samples,
        )],
    )?))
}

/// A completed six-second parent whose opening part has a real CMAF index.
pub fn write_cmaf_segment(
    lease: &StreamLease,
    local: u32,
    segment: u64,
    start: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    write_cmaf_segment_with_keyframes(lease, local, segment, start, &[0])
}

/// Select keyframe-bearing parts within a six-second parent.
pub fn write_cmaf_segment_with_keyframes(
    lease: &StreamLease,
    local: u32,
    segment: u64,
    start: i64,
    keys: &[u32],
) -> Result<(), Box<dyn std::error::Error>> {
    for index in 0..6 {
        let PackagedMedia::Chunk(mut part) = chunk(local, segment, index, start + i64::from(index))
        else {
            unreachable!()
        };
        part.payload = cmaf_fragment_at(keys.contains(&index), start + i64::from(index), 0)?;
        write(lease, PackagedMedia::Chunk(part));
    }
    write(
        lease,
        PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
            rendition_id: PackagingRenditionId(local),
            packaging_segment_id: PackagingSegmentId(segment),
            media_start: start,
            duration: 6,
        }),
    );
    Ok(())
}
