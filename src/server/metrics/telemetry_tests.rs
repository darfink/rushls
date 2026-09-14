use super::*;
use crate::{
    domain::{MediaKind, Payload},
    media::fixtures::video_presentation,
    mux::{
        InitializationSegment, PackagedChunk, PackagedMedia, PackagingRenditionId,
        PackagingSegmentId,
        fixtures::{RenditionBuilder, presentation},
    },
};
use std::error::Error;

#[tokio::test(start_paused = true)]
async fn committed_media_is_exported_and_disconnected_leases_stop_aging()
-> Result<(), Box<dyn Error>> {
    let store = StreamStore::default();
    let id = StreamId::new("live/camera");
    let plan = presentation(
        &video_presentation(),
        vec![RenditionBuilder::new(0, MediaKind::Video).build()],
    );
    let lease = store.lease(id.clone(), &plan)?;
    let endpoint = MetricsEndpoint::new(
        MetricsReader::new(
            ProcessMeters::default(),
            OriginMeters::default(),
            HlsMeters::default(),
            Registry::default(),
            store.clone(),
        ),
        None,
    );
    assert!(endpoint.render_streams().contains("rushls_rendition_output_started{stream=\"live/camera\",rendition=\"rendition/0\",kind=\"video\"} 0"));
    lease.write(PackagedMedia::Initialization(InitializationSegment {
        rendition_id: PackagingRenditionId(0),
        version: 1,
        payload: Payload::from(&b"init"[..]),
    }))?;
    let chunk = |index, start| {
        PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(0),
            packaging_segment_id: PackagingSegmentId(0),
            chunk_index: index,
            media_start: start,
            duration: 90_000,
            independent: true,
            payload: Payload::from(&b"media"[..]),
        })
    };
    lease.write(chunk(0, 0))?;
    tokio::time::advance(Duration::from_secs(3)).await;
    store.maintain();
    let rendered = endpoint.render_streams();
    assert!(rendered.contains("rushls_rendition_output_overdue_seconds{stream=\"live/camera\",rendition=\"rendition/0\",kind=\"video\"} 1"));
    assert!(
        endpoint
            .render()
            .contains("rushls_output_deadline_misses_total 1")
    );
    assert!(!endpoint.render().contains("rushls_packets_lost_total"));
    assert!(!endpoint.render().contains("rushls_bytes_served_total"));
    // Optional parser fixture for validating exposition with external tooling.
    if let Some(directory) = std::env::var_os("RUSHLS_METRICS_TEST_OUTPUT") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join("node.prom"), endpoint.render())?;
        std::fs::write(directory.join("streams.prom"), &rendered)?;
    }
    lease.publisher_disconnected();
    lease.write(chunk(1, 90_000))?;
    tokio::time::advance(Duration::from_secs(30)).await;
    let rendered = endpoint.render_streams();
    assert!(rendered.contains("rushls_stream_publisher_active{stream=\"live/camera\"} 0"));
    assert!(rendered.contains("rushls_rendition_output_overdue_seconds{stream=\"live/camera\",rendition=\"rendition/0\",kind=\"video\"} 0"));
    assert!(rendered.contains("rushls_rendition_media_published_seconds_total{stream=\"live/camera\",rendition=\"rendition/0\",kind=\"video\"} 2"));
    assert!(
        endpoint
            .render()
            .contains("rushls_output_deadline_misses_total 1")
    );
    Ok(())
}
