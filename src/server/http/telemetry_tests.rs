//! Exercise attribution through the same admission and body boundaries as HTTP.
use super::*;
use crate::{
    delivery::hls::{
        StreamStore,
        fixtures::{initialization, lease, video, write, write_segment},
    },
    domain::StreamId,
    observe::{
        HlsMeters, OriginMeters,
        http::{HttpFailure, HttpMethod, HttpResource},
    },
    server::metrics::MetricsReader,
    session::Registry,
};
use axum::http::Request;
use http_body_util::BodyExt;
use std::{error::Error, time::Duration};
use tower::ServiceExt;

#[tokio::test(start_paused = true)]
async fn stream_http_attributes_failures_and_bounds_label_lifetime() -> Result<(), Box<dyn Error>> {
    let store = StreamStore::default();
    let publication = lease(&store, vec![video(0)]);
    write(&publication, initialization(0, 1));
    write_segment(&publication, 0, 0, 0);
    let application = fixtures::application(&store);
    let reader = MetricsReader::new(
        ProcessMeters::default(),
        OriginMeters::default(),
        HlsMeters::default(),
        Registry::default(),
        store.clone(),
    );
    let totals = reader.http_meters();
    let budget = HttpBudget::new(HttpLimits::default()).with_meters(totals.clone());
    let endpoint = MetricsEndpoint::new(reader, None);
    let router = router(
        Arc::clone(&application),
        &HttpConfig::default(),
        None,
        Readiness::ready(),
        None,
    )
    .layer(axum::middleware::from_fn_with_state(
        (budget, application),
        limits::admit_application::<crate::server::runtime::ViewerApplication>,
    ));
    for (path, method, expected) in [
        ("/live/camera/0/segment/1.m4s", "GET", 200),
        ("/live/camera/0/segment/1.m4s", "HEAD", 200),
        ("/live/camera/0/init/999.mp4?token=secret", "GET", 404),
        ("/live/camera/999/video.m3u8", "GET", 404),
        ("/live/camera/0/video.m3u8?_HLS_msn=invalid", "GET", 400),
        ("/live/unpublished/index.m3u8", "GET", 404),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .method(method)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status().as_u16(), expected, "{path}");
        let body = response.into_body().collect().await?.to_bytes();
        if method == "HEAD" {
            assert!(body.is_empty());
        }
    }
    let live = store
        .get(&StreamId::new("live/camera"))
        .expect("published stream");
    let scoped = live.http_meters().snapshot();
    assert_eq!(
        scoped.responses[&(HttpResource::Media, HttpMethod::Get, 404)],
        1
    );
    assert_eq!(
        scoped.failures[&(HttpResource::Media, HttpFailure::UnknownResource)],
        1
    );
    assert_eq!(
        scoped.failures[&(HttpResource::Playlist, HttpFailure::UnknownRendition)],
        1
    );
    assert_eq!(
        scoped.failures[&(HttpResource::Playlist, HttpFailure::InvalidDirective)],
        1
    );
    assert_eq!(
        totals.snapshot().failures[&(HttpResource::Playlist, HttpFailure::UnknownStream)],
        1
    );
    let rendered = endpoint.render_streams();
    assert!(rendered.contains("rushls_stream_http_responses_total{stream=\"live/camera\",resource=\"media\",method=\"GET\",status=\"404\"} 1"));
    assert!(rendered.contains("rushls_stream_http_handler_duration_seconds_bucket{stream=\"live/camera\",resource=\"media\",le=\"+Inf\"} 3"));
    assert!(!rendered.contains("secret"));
    assert!(!rendered.contains("unpublished"));
    assert!(!endpoint.render().contains("rushls_stream_http_"));

    // Idle streams retain diagnostics for their normal playback lifetime.
    // Removing the stream must not leave a separate telemetry registry behind.
    publication.publisher_disconnected();
    drop(publication);
    tokio::time::advance(Duration::from_secs(3600)).await;
    store.maintain();
    assert!(!endpoint.render_streams().contains("rushls_stream_http_"));
    assert!(endpoint.render().contains("rushls_http_failures_total"));
    Ok(())
}
