//! HTTP tests over a real listener.
//!
//! Driven through an actual TCP socket rather than by calling the handler,
//! because the things worth testing at this layer are the ones a handler
//! signature hides: that HEAD produces no body, that a range comes back as 206
//! with the right `Content-Range`, that a blocked request keeps its connection
//! open, and that shutdown lets an in-flight blocking reload finish.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::delivery::hls::{
    StreamStore,
    fixtures::{
        PART_BYTES, chunk, initialization, lease, video, video_with_cadence, write, write_segment,
    },
    serve::{DeliveryConfig, Origin},
};

use super::{HttpConfig, bind, serve};

/// A raw HTTP/1.1 response, parsed just enough to assert on.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Speaks HTTP/1.1 by hand rather than pulling in a client crate.
///
/// Each request is a few lines, and a hand-written one can assert on exactly
/// what came back — including that a HEAD carried no body at all, which a
/// client library would hide behind its own response type.
async fn request(address: SocketAddr, method: &str, target: &str, extra: &[(&str, &str)]) -> Reply {
    let mut stream = TcpStream::connect(address).await.expect("the origin is up");
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: origin\r\nConnection: close\r\n");
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .expect("the request is sent");

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("the response completes");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has headers");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("the status line parses");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body,
    }
}

/// A running origin, its store, and the handle that stops it.
struct Harness {
    address: SocketAddr,
    store: StreamStore,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    served: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Harness {
    async fn start() -> Self {
        let store = StreamStore::default();
        let origin = Arc::new(Origin::new(store.clone(), DeliveryConfig::default()));
        let listener = bind("127.0.0.1:0".parse().expect("a valid address"))
            .await
            .expect("an ephemeral port is available");
        let address = listener.local_addr().expect("the listener is bound");
        let (shutdown, signal) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(serve(listener, origin, HttpConfig::default(), async {
            let _ = signal.await;
        }));
        Self {
            address,
            store,
            shutdown: Some(shutdown),
            served: Some(served),
        }
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(served) = self.served.take() {
            let _ = served.await;
        }
    }
}

#[tokio::test]
async fn a_playlist_is_served_with_its_media_type_and_no_caching() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let reply = request(harness.address, "GET", "/live/camera/0/media.m3u8", &[]).await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.header("content-type"),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(reply.header("cache-control"), Some("no-cache"));
    assert_eq!(reply.header("access-control-allow-origin"), Some("*"));
    let body = String::from_utf8(reply.body).expect("a playlist is text");
    assert!(body.starts_with("#EXTM3U\n"));
    assert!(body.contains("segment/1.m4s"));

    harness.stop().await;
}

#[tokio::test]
async fn a_multivariant_playlist_names_its_renditions() {
    let harness = Harness::start().await;
    let _lease = lease(&harness.store, vec![video(0)]);

    let reply = request(harness.address, "GET", "/live/camera/master.m3u8", &[]).await;

    assert_eq!(reply.status, 200);
    let body = String::from_utf8(reply.body).expect("a playlist is text");
    assert!(body.contains("#EXT-X-STREAM-INF:"));
    assert!(body.contains("\n0/media.m3u8\n"));

    harness.stop().await;
}

#[tokio::test]
async fn a_segment_is_immutable_and_carries_its_true_length() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let reply = request(harness.address, "GET", "/live/camera/0/segment/1.m4s", &[]).await;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type"), Some("video/iso.segment"));
    assert_eq!(
        reply.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(reply.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        reply.body.len(),
        6 * PART_BYTES,
        "the six stored parts arrive as one segment without being reassembled \
         in memory first"
    );
    assert_eq!(
        reply.header("content-length"),
        Some((6 * PART_BYTES).to_string().as_str())
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_head_request_answers_with_the_headers_and_no_body() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let reply = request(harness.address, "HEAD", "/live/camera/0/segment/1.m4s", &[]).await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.header("content-length"),
        Some((6 * PART_BYTES).to_string().as_str()),
        "a client sizing a resource must be told the truth without fetching it"
    );
    assert!(reply.body.is_empty());

    harness.stop().await;
}

#[tokio::test]
async fn a_byte_range_is_answered_from_the_buffers_it_spans() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    let total = 6 * PART_BYTES;

    let reply = request(
        harness.address,
        "GET",
        "/live/camera/0/segment/1.m4s",
        &[("Range", "bytes=1000-2100")],
    )
    .await;

    assert_eq!(reply.status, 206);
    assert_eq!(
        reply.header("content-range"),
        Some(format!("bytes 1000-2100/{total}").as_str())
    );
    assert_eq!(reply.body.len(), 1_101);
    assert_eq!(
        &reply.body[..24],
        &vec![0_u8; 24][..],
        "the clipped head still comes from the first part's own buffer"
    );
    assert_eq!(&reply.body[24..26], &[1, 1]);

    let refused = request(
        harness.address,
        "GET",
        "/live/camera/0/segment/1.m4s",
        &[("Range", "bytes=99999-")],
    )
    .await;
    assert_eq!(refused.status, 416);

    harness.stop().await;
}

#[tokio::test]
async fn unknown_resources_and_bad_directives_are_told_apart() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    for (target, expected) in [
        ("/nobody/here/master.m3u8", 404),
        ("/live/camera/9/media.m3u8", 404),
        ("/live/camera/0/segment/999.m4s", 404),
        ("/live/camera/0/media.m3u8?_HLS_part=2", 400),
        ("/live/camera/0/media.m3u8?_HLS_msn=9999", 400),
    ] {
        let reply = request(harness.address, "GET", target, &[]).await;
        assert_eq!(reply.status, expected, "{target}");
    }

    let rejected = request(harness.address, "POST", "/live/camera/master.m3u8", &[]).await;
    assert_eq!(rejected.status, 405);
    assert_eq!(rejected.header("allow"), Some("GET, HEAD"));

    harness.stop().await;
}

#[tokio::test]
async fn a_blocking_reload_holds_the_connection_until_its_part_arrives() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    write(&lease, chunk(0, 1, 0, 6));

    let address = harness.address;
    let held = tokio::spawn(async move {
        request(
            address,
            "GET",
            "/live/camera/0/media.m3u8?_HLS_msn=1&_HLS_part=1",
            &[],
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!held.is_finished(), "the part has not been published yet");

    write(&lease, chunk(0, 1, 1, 7));
    let reply = held.await.expect("the request task ran");

    assert_eq!(reply.status, 200);
    let body = String::from_utf8(reply.body).expect("a playlist is text");
    assert!(
        body.contains("URI=\"part/8.m4s\""),
        "the response carries the part the client blocked for: {body}"
    );

    harness.stop().await;
}

#[tokio::test]
async fn an_unsatisfiable_wait_ends_in_503_with_a_retry_hint() {
    let harness = Harness::start().await;
    // A one-second cadence, because this test has to sit through the whole
    // deadline and that deadline is three target durations by definition.
    let lease = lease(&harness.store, vec![video_with_cadence(0, 1, 1)]);
    write(&lease, initialization(0, 1));
    write(&lease, chunk(0, 0, 0, 0));
    write(
        &lease,
        crate::mux::PackagedMedia::SegmentCompleted(crate::mux::PackagedSegmentCompletion {
            rendition_id: crate::mux::PackagingRenditionId(0),
            packaging_segment_id: crate::mux::PackagingSegmentId(0),
            media_start: 0,
            duration: 1,
        }),
    );

    let reply = request(
        harness.address,
        "GET",
        "/live/camera/0/media.m3u8?_HLS_msn=2",
        &[],
    )
    .await;

    assert_eq!(reply.status, 503);
    assert_eq!(
        reply.header("retry-after"),
        Some("1"),
        "a deadline passing means the origin fell behind, not that the stream \
         is over, so the client is told to come back"
    );

    harness.stop().await;
}

#[tokio::test]
async fn shutdown_lets_an_in_flight_blocking_reload_finish() {
    let mut harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);
    write(&lease, chunk(0, 1, 0, 6));

    let address = harness.address;
    let held = tokio::spawn(async move {
        request(
            address,
            "GET",
            "/live/camera/0/media.m3u8?_HLS_msn=1&_HLS_part=1",
            &[],
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shutdown begins while a viewer is parked mid-reload. Dropping it here is
    // what turns a routine restart into a visible stall for every viewer.
    let shutdown = harness.shutdown.take().expect("the harness holds it");
    let served = harness.served.take().expect("the harness holds it");
    let _ = shutdown.send(());

    write(&lease, chunk(0, 1, 1, 7));
    let reply = held.await.expect("the request task ran");
    assert_eq!(reply.status, 200);

    let _ = served.await;
}
