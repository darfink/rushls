use super::*;
use axum::routing::get;
use http_body_util::BodyExt;
use std::error::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use tower::ServiceExt;

fn budget() -> HttpBudget {
    HttpBudget::new(HttpLimits {
        maximum_connections: 1,
        maximum_requests: 1,
    })
}

fn limited(router: Router, budget: &HttpBudget) -> Router {
    router.layer(axum::middleware::from_fn_with_state(budget.clone(), admit))
}

fn request() -> Request {
    Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("static request")
}

#[tokio::test]
async fn response_body_holds_capacity_during_overload_and_releases_on_drop()
-> Result<(), Box<dyn Error>> {
    let budget = budget();
    let router = limited(Router::new().route("/", get(|| async { "media" })), &budget);
    let response = router.clone().oneshot(request()).await?;
    let mut requests = JoinSet::new();
    for _ in 0..100 {
        requests.spawn(router.clone().oneshot(request()));
    }
    while let Some(response) = requests.join_next().await {
        let response = response??;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    assert_eq!(budget.requests.available_permits(), 0);
    drop(response);
    let response = router.oneshot(request()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await?.to_bytes(), "media");
    assert_eq!(budget.requests.available_permits(), 1);
    Ok(())
}

#[tokio::test]
async fn stream_http_admission_refusal_keeps_status_and_reason() -> Result<(), Box<dyn Error>> {
    let budget = budget();
    let scoped = HttpMeters::default();
    let router = limited(Router::new().route("/", get(|| async { "media" })), &budget);
    let held = router.clone().oneshot(request()).await?;
    let mut refused = request();
    refused.extensions_mut().insert(scoped.clone());
    let response = router.oneshot(refused).await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let snapshot = scoped.snapshot();
    assert_eq!(
        snapshot.responses[&(HttpResource::Other, HttpMethod::Get, 503)],
        1
    );
    assert_eq!(
        snapshot.failures[&(HttpResource::Other, HttpFailure::Admission)],
        1
    );
    assert_eq!(snapshot.classes[HttpResource::Other as usize].completed, 1);
    assert_eq!(snapshot.classes[HttpResource::Other as usize].in_flight, 0);
    assert_eq!(budget.meters.snapshot().requests_rejected, 1);
    drop(held);
    Ok(())
}

#[tokio::test]
async fn cancelled_blocking_request_releases_capacity() -> Result<(), Box<dyn Error>> {
    let budget = budget();
    let started = Arc::new(Notify::new());
    let signal = started.clone();
    let router = limited(
        Router::new().route(
            "/",
            get(move || {
                signal.notify_one();
                std::future::pending::<StatusCode>()
            }),
        ),
        &budget,
    );
    let held = tokio::spawn(router.clone().oneshot(request()));
    tokio::time::timeout(Duration::from_secs(1), started.notified()).await?;
    assert_eq!(
        router.oneshot(request()).await?.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    held.abort();
    assert!(held.await.expect_err("request cancelled").is_cancelled());
    assert_eq!(budget.requests.available_permits(), 1);
    Ok(())
}

async fn fetch_headers(socket: &mut TcpStream) -> Result<String, Box<dyn Error>> {
    socket
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !response.ends_with(b"\r\n\r\n") {
            response.push(socket.read_u8().await?);
        }
        std::io::Result::Ok(())
    })
    .await??;
    Ok(String::from_utf8(response)?)
}

#[tokio::test]
async fn connection_budget_is_shared_between_listeners_and_recovers() -> Result<(), Box<dyn Error>>
{
    let budget = budget();
    let first = TcpListener::bind("127.0.0.1:0").await?;
    let second = TcpListener::bind("127.0.0.1:0").await?;
    let first_address = first.local_addr()?;
    let second_address = second.local_addr()?;
    let router = Router::new().route("/", get(|| async { StatusCode::NO_CONTENT }));
    let (stop_a, stopped_a) = oneshot::channel();
    let (stop_b, stopped_b) = oneshot::channel();
    let a = tokio::spawn(serve(
        crate::server::http::TcpHttpListener::from(first),
        router.clone(),
        budget.clone(),
        async {
            let _ = stopped_a.await;
        },
    ));
    let b = tokio::spawn(serve(
        crate::server::http::TcpHttpListener::from(second),
        router,
        budget.clone(),
        async {
            let _ = stopped_b.await;
        },
    ));
    let mut held = TcpStream::connect(first_address).await?;
    assert!(fetch_headers(&mut held).await?.starts_with("HTTP/1.1 204"));
    let mut rejected = TcpStream::connect(second_address).await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), rejected.read(&mut [0])).await??,
        0
    );
    drop(held);
    // Acquiring and releasing is a deterministic fence on the old socket's drop.
    drop(tokio::time::timeout(Duration::from_secs(2), budget.connections.acquire()).await??);
    let mut accepted = TcpStream::connect(second_address).await?;
    assert!(
        fetch_headers(&mut accepted)
            .await?
            .starts_with("HTTP/1.1 204")
    );
    drop(accepted);
    let _ = stop_a.send(());
    let _ = stop_b.send(());
    tokio::time::timeout(Duration::from_secs(2), a).await???;
    tokio::time::timeout(Duration::from_secs(2), b).await???;
    assert_eq!(budget.connections.available_permits(), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn partial_protocol_preface_has_a_deadline() -> Result<(), Box<dyn Error>> {
    let budget = budget();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(serve(
        crate::server::http::TcpHttpListener::from(listener),
        Router::new(),
        budget.clone(),
        std::future::pending(),
    ));
    let mut socket = TcpStream::connect(address).await?;
    socket.write_all(b"PRI * ").await?;
    // Allow accept to run without advancing the paused clock.
    while budget.connections.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(31)).await;
    let result = socket.read(&mut [0]).await;
    assert!(matches!(result, Ok(0)) || result.is_err());
    drop(budget.connections.acquire().await?);
    server.abort();
    let _ = server.await;
    assert_eq!(budget.connections.available_permits(), 1);
    Ok(())
}

#[tokio::test]
async fn http2_multiplexing_obeys_the_request_budget() -> Result<(), Box<dyn Error>> {
    let budget = budget();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let signal = started.clone();
    let release = released.clone();
    let router = limited(
        Router::new().route(
            "/",
            get(move || {
                let signal = signal.clone();
                let release = release.clone();
                async move {
                    signal.notify_one();
                    release.notified().await;
                    StatusCode::NO_CONTENT
                }
            }),
        ),
        &budget,
    );
    let server = tokio::spawn(serve(
        crate::server::http::TcpHttpListener::from(listener),
        router,
        budget.clone(),
        std::future::pending(),
    ));
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http::<Body>();
    let uri: http::Uri = format!("http://{address}/").parse()?;
    let first = tokio::spawn(client.get(uri.clone()));
    tokio::time::timeout(Duration::from_secs(2), started.notified()).await?;
    let second = tokio::time::timeout(Duration::from_secs(2), client.get(uri)).await??;
    assert_eq!(second.version(), http::Version::HTTP_2);
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(budget.connections.available_permits(), 0);
    released.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(2), first).await???;
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    server.abort();
    let _ = server.await;
    // Aborting the server must also stop its connection tasks.
    drop(tokio::time::timeout(Duration::from_secs(2), budget.connections.acquire()).await??);
    Ok(())
}

#[tokio::test]
async fn body_metrics_count_only_consumed_frames_and_cancellation() -> Result<(), Box<dyn Error>> {
    let budget = budget();
    let scoped = HttpMeters::default();
    let scoped_request = || {
        let mut request = request();
        request.extensions_mut().insert(scoped.clone());
        request
    };
    let router = limited(Router::new().route("/", get(|| async { "media" })), &budget);
    let response = router.clone().oneshot(scoped_request()).await?;
    assert_eq!(
        budget.meters.snapshot().classes[HttpResource::Other as usize].body_bytes,
        0
    );
    drop(response);
    let response = router.oneshot(scoped_request()).await?;
    assert_eq!(response.into_body().collect().await?.to_bytes(), "media");
    let snapshot = budget.meters.snapshot();
    let class = &snapshot.classes[HttpResource::Other as usize];
    assert_eq!(class.body_bytes, 5);
    assert_eq!(class.cancelled, 1);
    assert_eq!(class.completed, 1);
    assert_eq!(class.in_flight, 0);
    assert_eq!(
        snapshot.responses[&(HttpResource::Other, HttpMethod::Get, 200)],
        2
    );
    let stream = scoped.snapshot();
    let stream_class = &stream.classes[HttpResource::Other as usize];
    assert_eq!(stream_class.body_bytes, class.body_bytes);
    assert_eq!(stream_class.cancelled, 1);
    assert_eq!(stream_class.completed, 1);
    assert_eq!(stream_class.in_flight, 0);
    assert_eq!(stream_class.body_duration.count, 2);
    Ok(())
}

#[tokio::test]
async fn conditional_and_ranged_responses_do_not_count_the_original_object()
-> Result<(), Box<dyn Error>> {
    use crate::{
        delivery::{
            Body as DeliveryBody, MediaBody, Response as DeliveryResponse, Reuse, uri::ContentType,
        },
        domain::Payload,
    };
    let budget = budget();
    let router = limited(
        Router::new().route(
            "/media.vtt",
            get(|headers: axum::http::HeaderMap| async move {
                let response = DeliveryResponse {
                    body: DeliveryBody::Media(MediaBody::single(Payload::from(&b"0123456789"[..]))),
                    gzip: Some(crate::delivery::hls::gzip::gzip(&Bytes::from_static(
                        b"0123456789",
                    ))),
                    content_type: ContentType::WebVtt,
                    reuse: Reuse::immutable(Duration::from_secs(60)),
                };
                super::super::into_http(
                    response,
                    headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
                    headers.contains_key(header::ACCEPT_ENCODING),
                    headers
                        .get(header::IF_NONE_MATCH)
                        .and_then(|v| v.to_str().ok()),
                    true,
                )
                .unwrap_or_else(IntoResponse::into_response)
            }),
        ),
        &budget,
    );
    let request = |range: Option<&str>, etag: Option<&str>| {
        let mut builder = Request::builder().uri("/media.vtt");
        if let Some(range) = range {
            builder = builder.header(header::RANGE, range);
        }
        if let Some(etag) = etag {
            builder = builder.header(header::IF_NONE_MATCH, etag);
        }
        builder.body(Body::empty())
    };
    let response = router
        .clone()
        .oneshot(request(Some("bytes=2-4"), None)?)
        .await?;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let etag = response.headers()[header::ETAG].to_str()?.to_owned();
    assert_eq!(response.into_body().collect().await?.to_bytes(), "234");
    let response = router.clone().oneshot(request(None, Some(&etag))?).await?;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(response.into_body().collect().await?.to_bytes().is_empty());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media.vtt")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())?,
        )
        .await?;
    let gzip_bytes = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        gzip_bytes,
        crate::delivery::hls::gzip::gzip(&Bytes::from_static(b"0123456789"))
    );
    let response = router
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/media.vtt")
                .body(Body::empty())?,
        )
        .await?;
    assert!(response.into_body().collect().await?.to_bytes().is_empty());
    let snapshot = budget.meters.snapshot();
    assert_eq!(
        snapshot.classes[HttpResource::Media as usize].body_bytes,
        3 + gzip_bytes.len() as u64
    );
    assert_eq!(
        snapshot.responses[&(HttpResource::Media, HttpMethod::Get, 206)],
        1
    );
    assert_eq!(
        snapshot.responses[&(HttpResource::Media, HttpMethod::Get, 304)],
        1
    );
    Ok(())
}
