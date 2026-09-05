//! HTTP tests over a real listener.
//!
//! Driven through an actual TCP socket rather than by calling the handler,
//! because the things worth testing at this layer are the ones a handler
//! signature hides: that HEAD produces no body, that a range comes back as 206
//! with the right `Content-Range`, that a blocked request keeps its connection
//! open, and that shutdown lets an in-flight blocking reload finish.

use std::{fmt::Write as _, net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::delivery::hls::{
    StreamStore,
    fixtures::{
        PART_BYTES, chunk, initialization, lease, video, video_with_cadence, write, write_segment,
    },
};
use crate::{
    observe::{HlsMeters, OriginMeters, ProcessMeters},
    server::metrics::{ExportPolicy, MetricsEndpoint, MetricsReader, MetricsToken},
    session::Registry,
};

use super::fixtures::application;
use super::{AllowedOrigins, CorsConfig, HttpConfig, OriginPattern, Readiness, serve};

/// A raw HTTP/1.1 response, parsed just enough to assert on.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    /// Every value sent for one field name, joined the way a comma-separated
    /// list field means them.
    ///
    /// `Vary` arrives as more than one line here: the origin sets its own
    /// content-negotiation field and the CORS middleware adds its own, which
    /// HTTP treats as equivalent to one combined list.
    fn joined(&self, name: &str) -> Option<String> {
        let values: Vec<&str> = self
            .headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    }

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
        let _ = write!(head, "{name}: {value}\r\n");
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

/// Several requests down one kept-alive connection, framed only by
/// `Content-Length`.
///
/// This is libavformat's HLS reader in miniature: it reissues each playlist
/// request on the connection it already has, and frames every response by its
/// declared length. A response that arrives without one is `Transfer-Encoding:
/// chunked`, which this cannot frame — and neither could the real client, which
/// read the *following* playlist as garbage and rejected it as invalid data.
async fn over_one_connection(address: SocketAddr, targets: &[&str]) -> Vec<Reply> {
    let mut stream = TcpStream::connect(address).await.expect("the origin is up");
    let mut pending = Vec::new();
    let mut replies = Vec::with_capacity(targets.len());
    for target in targets {
        let head =
            format!("GET {target} HTTP/1.1\r\nHost: origin\r\nConnection: keep-alive\r\n\r\n");
        stream
            .write_all(head.as_bytes())
            .await
            .expect("the request is sent");
        replies.push(read_declared(&mut stream, &mut pending).await);
    }
    replies
}

/// Reads exactly one length-delimited response, leaving any surplus for the
/// next one.
async fn read_declared(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Reply {
    let split = loop {
        if let Some(at) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("the response arrives");
        assert!(read > 0, "the origin closed before sending headers");
        pending.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&pending[..split]).to_string();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("the status line parses");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let Some((_, declared)) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    else {
        panic!("a response with no declared length cannot be framed on a reused connection: {head}")
    };
    let length: usize = declared.parse().expect("a declared length is a number");

    let start = split + 4;
    while pending.len() < start + length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("the body arrives");
        assert!(read > 0, "the origin closed mid-body");
        pending.extend_from_slice(&chunk[..read]);
    }
    let body = pending[start..start + length].to_vec();
    pending.drain(..start + length);
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

enum MetricsMode {
    Disabled,
    Enabled(Option<MetricsToken>),
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(HttpConfig::default()).await
    }

    async fn start_with(config: HttpConfig) -> Self {
        Self::start_config(config, MetricsMode::Disabled).await
    }

    async fn start_with_metrics(config: HttpConfig, token: Option<MetricsToken>) -> Self {
        Self::start_config(config, MetricsMode::Enabled(token)).await
    }

    async fn start_config(config: HttpConfig, metrics: MetricsMode) -> Self {
        Self::start_config_with_readiness(config, metrics, Readiness::ready()).await
    }

    async fn start_with_readiness(readiness: Readiness) -> Self {
        Self::start_config_with_readiness(HttpConfig::default(), MetricsMode::Disabled, readiness)
            .await
    }

    async fn start_config_with_readiness(
        config: HttpConfig,
        metrics: MetricsMode,
        readiness: Readiness,
    ) -> Self {
        let store = StreamStore::default();
        let origin = application(&store);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port is available");
        let address = listener.local_addr().expect("the listener is bound");
        let (shutdown, signal) = tokio::sync::oneshot::channel();
        let metrics = match metrics {
            MetricsMode::Enabled(token) => Some(MetricsEndpoint::new(
                MetricsReader::new(
                    ProcessMeters::default(),
                    OriginMeters::default(),
                    HlsMeters::default(),
                    Registry::default(),
                    store.clone(),
                    ExportPolicy::default(),
                ),
                token,
            )),
            MetricsMode::Disabled => None,
        };
        let served = tokio::spawn(serve(listener, origin, config, metrics, readiness, async {
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
async fn liveness_is_always_available_and_readiness_tracks_runtime_state() {
    let readiness = Readiness::default();
    let harness = Harness::start_with_readiness(readiness.clone()).await;

    let live = request(harness.address, "GET", "/health/live", &[]).await;
    assert_eq!(live.status, 200);
    assert_eq!(live.body, b"ok\n");
    assert_eq!(live.header("cache-control"), Some("no-store"));

    let starting = request(harness.address, "GET", "/health/ready", &[]).await;
    assert_eq!(starting.status, 503);
    assert_eq!(starting.body, b"not ready\n");

    readiness.mark_ready();
    let ready = request(harness.address, "HEAD", "/health/ready", &[]).await;
    assert_eq!(ready.status, 200);
    assert!(ready.body.is_empty());

    readiness.mark_not_ready();
    let stopping = request(harness.address, "GET", "/health/ready", &[]).await;
    assert_eq!(stopping.status, 503);

    harness.stop().await;
}

#[tokio::test]
async fn health_endpoints_reject_methods_that_cannot_be_probes() {
    let harness = Harness::start().await;

    let reply = request(harness.address, "POST", "/health/live", &[]).await;

    assert_eq!(reply.status, 405);
    assert_eq!(reply.header("allow"), Some("GET, HEAD"));
    harness.stop().await;
}

#[tokio::test]
async fn enabled_metrics_can_be_exposed_without_authentication() {
    let harness = Harness::start_with_metrics(HttpConfig::default(), None).await;

    let reply = request(harness.address, "GET", "/metrics", &[]).await;

    assert_eq!(reply.status, 200);
    assert!(
        String::from_utf8(reply.body)
            .expect("metrics are UTF-8")
            .contains("rushls_active_sessions 0\n")
    );
    harness.stop().await;
}

#[tokio::test]
async fn metrics_are_absent_unless_the_endpoint_is_enabled() {
    let harness = Harness::start().await;

    let reply = request(harness.address, "GET", "/metrics", &[]).await;

    assert_eq!(reply.status, 404);
    harness.stop().await;
}

#[tokio::test]
async fn metrics_require_the_configured_bearer_token() {
    let harness = Harness::start_with_metrics(
        HttpConfig::default(),
        Some(MetricsToken::new("scrape-secret")),
    )
    .await;

    let missing = request(harness.address, "GET", "/metrics", &[]).await;
    assert_eq!(missing.status, 401);
    assert_eq!(missing.header("www-authenticate"), Some("Bearer"));

    let wrong = request(
        harness.address,
        "GET",
        "/metrics",
        &[("Authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(wrong.status, 401);

    let accepted = request(
        harness.address,
        "GET",
        "/metrics",
        &[("Authorization", "Bearer scrape-secret")],
    )
    .await;
    assert_eq!(accepted.status, 200);
    assert_eq!(
        accepted.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    assert_eq!(accepted.header("cache-control"), Some("no-store"));
    let body = String::from_utf8(accepted.body).expect("metrics are UTF-8");
    assert!(body.contains("rushls_active_sessions 0\n"));

    harness.stop().await;
}

#[tokio::test]
async fn a_playlist_is_served_with_its_media_type_and_half_a_target_duration() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let reply = request(harness.address, "GET", "/live/camera/0/video.m3u8", &[]).await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.header("content-type"),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(
        reply.header("cache-control"),
        Some("public, max-age=3, no-transform"),
        "a plain reload names the live edge, so it may be reused for half of \
         the fixture's six-second target duration, and the text it holds has \
         two codings an intermediary must not convert between"
    );
    assert_eq!(reply.header("access-control-allow-origin"), Some("*"));
    let body = String::from_utf8(reply.body).expect("a playlist is text");
    assert!(body.starts_with("#EXTM3U\n"));
    assert!(body.contains("segment/1.m4s"));

    harness.stop().await;
}

/// Decompresses a response body the origin said was gzipped.
fn ungzip(body: &[u8]) -> Vec<u8> {
    use std::io::Read;

    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(body)
        .read_to_end(&mut decoded)
        .expect("the origin emits a well-formed gzip member");
    decoded
}

/// Both playlist encodings must carry a length out of this layer, whichever
/// transport later serializes them.
///
/// Asserted here rather than through the listener because HTTP/1.1 hides the
/// bug: hyper derives a length from a fully buffered body's exact size hint, so
/// a cleartext request looks correct either way. Over HTTP/2 — which is what
/// this origin negotiates under TLS, and what the Low-Latency profile expects —
/// framing is by `END_STREAM` and no length is synthesized. A proxy converting
/// that back to HTTP/1.1 for an older client then has nothing to declare and
/// must chunk, which is what broke libavformat's connection reuse in
/// production. So the invariant is that *this* layer states the length, not
/// that some transport happens to.
#[test]
fn a_playlist_leaves_this_layer_carrying_its_own_length() {
    use bytes::Bytes;

    use crate::delivery::{Response as DeliveryResponse, Reuse, uri::ContentType};

    let playlist = Bytes::from_static(b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n");
    let gzipped = Bytes::from_static(b"pretend this is a gzip member");
    let response = || {
        DeliveryResponse::manifest(
            playlist.clone(),
            gzipped.clone(),
            ContentType::Manifest("application/vnd.apple.mpegurl"),
            Reuse::revalidate(),
        )
    };

    let identity =
        super::into_http(response(), None, false, None).expect("a playlist is representable");
    assert_eq!(
        identity
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .map(|value| value.to_str().expect("a length is ASCII").to_owned()),
        Some(playlist.len().to_string()),
        "an undeclared length becomes a chunked response, and a client reusing \
         its connection mis-frames everything after it"
    );

    let encoded =
        super::into_http(response(), None, true, None).expect("a playlist is representable");
    assert_eq!(
        encoded
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .map(|value| value.to_str().expect("a length is ASCII").to_owned()),
        Some(gzipped.len().to_string()),
        "the declared length is the encoded bytes on the wire, not the playlist \
         they decompress to"
    );
}

/// The two encodings of one playlist must not validate as the same entity.
///
/// A shared cache holding a gzip copy and asked for identity may answer from
/// what it has when nothing distinguishes the two. Observed in production
/// behind a CDN that recomputed the range against its compressed copy and then
/// decompressed the body, producing `Content-Range: bytes 0-370/371` above 852
/// bytes of playlist. A client reusing an HTTP/1.1 connection reads the excess
/// as the next response's headers.
#[test]
fn each_playlist_encoding_validates_as_a_different_entity() {
    use bytes::Bytes;

    use crate::delivery::{Response as DeliveryResponse, Reuse, uri::ContentType};

    let playlist = Bytes::from_static(b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n");
    let gzipped = crate::delivery::hls::gzip::gzip(&playlist);
    let response = || {
        DeliveryResponse::manifest(
            playlist.clone(),
            gzipped.clone(),
            ContentType::Manifest("application/vnd.apple.mpegurl"),
            Reuse::revalidate(),
        )
    };
    let etag = |response: &axum::response::Response| {
        response
            .headers()
            .get(axum::http::header::ETAG)
            .map(|value| value.to_str().expect("an entity tag is ASCII").to_owned())
    };

    let identity =
        super::into_http(response(), None, false, None).expect("a playlist is representable");
    let encoded =
        super::into_http(response(), None, true, None).expect("a playlist is representable");

    let (identity, encoded) = (etag(&identity), etag(&encoded));
    assert!(identity.is_some(), "a playlist carries a validator");
    assert_ne!(
        identity, encoded,
        "a cache that cannot tell the encodings apart may answer a request for \
         one with the other"
    );
    assert!(
        encoded.is_some_and(|tag| tag.ends_with("-gz\"")),
        "the encoded representation is the one marked, so the identity tag \
         stays stable for clients that never negotiate"
    );
}

/// A viewer holding current bytes is told so rather than sent them again.
///
/// A playlist is reusable for half a target duration, so this is the common
/// case for the whole of a session, not an edge case.
#[tokio::test]
async fn an_unchanged_playlist_revalidates_without_its_body() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let first = request(harness.address, "GET", "/live/camera/0/video.m3u8", &[]).await;
    let tag = first
        .header("etag")
        .expect("a playlist carries a validator");

    let revalidated = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("If-None-Match", tag)],
    )
    .await;

    assert_eq!(revalidated.status, 304);
    assert!(revalidated.body.is_empty());
    assert_eq!(revalidated.header("etag"), Some(tag));
    assert_eq!(
        revalidated.header("content-length"),
        None,
        "a 304 is framed by its status, and a length here is what an \
         intermediary turns into a body that was never sent"
    );

    // A weak validator names the same representation for caching purposes.
    let weak = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("If-None-Match", &format!("W/{tag}"))],
    )
    .await;
    assert_eq!(weak.status, 304);

    let stale = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("If-None-Match", "\"00000000-0\"")],
    )
    .await;
    assert_eq!(
        stale.status, 200,
        "a validator naming other bytes is answered with the current ones"
    );
    assert_eq!(stale.body, first.body);

    harness.stop().await;
}

/// Revalidation is per-representation, or it hands over the wrong bytes.
///
/// The identity tag presented on a gzip-accepting request describes a
/// different representation, so answering 304 would leave the client using
/// uncompressed bytes it believes are current under a compressed entity — the
/// same conflation that produced the mismatched lengths behind the CDN.
#[tokio::test]
async fn revalidating_one_encoding_never_answers_for_the_other() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let identity = request(harness.address, "GET", "/live/camera/0/video.m3u8", &[]).await;
    let encoded = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "gzip")],
    )
    .await;

    let (identity_tag, encoded_tag) = (
        identity.header("etag").expect("a validator"),
        encoded.header("etag").expect("a validator"),
    );

    let crossed = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "gzip"), ("If-None-Match", identity_tag)],
    )
    .await;
    assert_eq!(
        crossed.status, 200,
        "the identity validator does not describe the encoded representation"
    );
    assert_eq!(crossed.header("content-encoding"), Some("gzip"));

    let matched = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "gzip"), ("If-None-Match", encoded_tag)],
    )
    .await;
    assert_eq!(matched.status, 304);

    harness.stop().await;
}

/// A playlist is not range-addressable, and says so.
///
/// The manifest branch already ignores `Range` and answers 200. Stating
/// `Accept-Ranges: none` denies an intermediary the premise for synthesizing a
/// 206 it was never offered, and `no-transform` asks it not to convert between
/// the codings in the first place.
#[test]
fn a_playlist_refuses_ranges_and_asks_not_to_be_transformed() {
    use bytes::Bytes;

    use crate::delivery::{Response as DeliveryResponse, Reuse, uri::ContentType};

    let playlist = Bytes::from_static(b"#EXTM3U\n#EXT-X-TARGETDURATION:6\n");
    let gzipped = crate::delivery::hls::gzip::gzip(&playlist);
    let response = || {
        DeliveryResponse::manifest(
            playlist.clone(),
            gzipped.clone(),
            ContentType::Manifest("application/vnd.apple.mpegurl"),
            Reuse::reusable(std::time::Duration::from_secs(3)),
        )
    };

    for accepts_gzip in [false, true] {
        // The range is offered exactly as libavformat sends it.
        let reply = super::into_http(response(), Some("bytes=0-"), accepts_gzip, None)
            .expect("a playlist is representable");

        assert_eq!(
            reply.status(),
            axum::http::StatusCode::OK,
            "a whole playlist is a valid answer to a range request, and the \
             only one this origin gives"
        );
        assert_eq!(
            reply
                .headers()
                .get(axum::http::header::ACCEPT_RANGES)
                .and_then(|value| value.to_str().ok()),
            Some("none")
        );
        assert_eq!(
            reply.headers().get(axum::http::header::CONTENT_RANGE),
            None,
            "no partial response is ever offered for a playlist"
        );
        assert!(
            reply
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("no-transform")),
            "an intermediary must not convert between the two codings"
        );
        assert_eq!(
            reply.headers().get(axum::http::header::CONTENT_ENCODING),
            None,
            "a range request is answered in identity, so its offsets and the \
             bytes on the wire describe the same representation"
        );
    }
}

/// Media keeps the range support a player depends on.
///
/// The playlist changes above must not leak onto segments: a byte-range
/// playlist addresses parts of a segment by offset, and refusing those would
/// break the profile outright.
#[tokio::test]
async fn media_still_advertises_and_serves_ranges() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let whole = request(harness.address, "GET", "/live/camera/0/segment/1.m4s", &[]).await;
    let partial = request(
        harness.address,
        "GET",
        "/live/camera/0/segment/1.m4s",
        &[("Range", "bytes=0-9")],
    )
    .await;

    assert_eq!(whole.header("accept-ranges"), Some("bytes"));
    assert_eq!(partial.status, 206);
    assert_eq!(partial.body.len(), 10);
    assert!(
        whole
            .header("cache-control")
            .is_some_and(|value| !value.contains("no-transform")),
        "media has one representation, so there is no transformation to refuse"
    );

    harness.stop().await;
}

#[tokio::test]
async fn every_playlist_declares_its_length_so_a_reused_connection_stays_framed() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    // A client that keeps its connection reads the multivariant playlist, then
    // the media playlist it names, then reloads it — all without reconnecting.
    let replies = over_one_connection(
        harness.address,
        &[
            "/live/camera/index.m3u8",
            "/live/camera/0/video.m3u8",
            "/live/camera/0/video.m3u8",
        ],
    )
    .await;

    for reply in &replies {
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.header("content-length"),
            Some(reply.body.len().to_string().as_str()),
            "a declared length that disagreed with the body would desynchronize \
             the connection just as surely as omitting it"
        );
        assert_eq!(
            reply.header("transfer-encoding"),
            None,
            "a fully buffered playlist has no reason to be chunked"
        );
        assert!(
            reply.body.starts_with(b"#EXTM3U"),
            "a mis-framed response begins mid-playlist, which is what a client \
             reports as invalid data: {:?}",
            String::from_utf8_lossy(&reply.body[..reply.body.len().min(32)])
        );
    }

    harness.stop().await;
}

#[tokio::test]
async fn a_gzipped_playlist_also_declares_its_length() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let encoded = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "gzip")],
    )
    .await;

    assert_eq!(encoded.header("content-encoding"), Some("gzip"));
    assert_eq!(
        encoded.header("content-length"),
        Some(encoded.body.len().to_string().as_str()),
        "the length describes the encoded bytes on the wire, not the playlist \
         they decompress to"
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_playlist_is_gzipped_for_a_client_that_accepts_it() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let plain = request(harness.address, "GET", "/live/camera/0/video.m3u8", &[]).await;
    let encoded = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "gzip, deflate")],
    )
    .await;

    assert_eq!(
        plain.header("content-encoding"),
        None,
        "silence is not an indication that a client accepts gzip"
    );
    assert_eq!(encoded.header("content-encoding"), Some("gzip"));
    assert_eq!(
        encoded.header("content-type"),
        Some("application/vnd.apple.mpegurl"),
        "the media type describes the playlist, not the transfer encoding"
    );
    assert_eq!(
        encoded.joined("vary").as_deref(),
        Some("Accept-Encoding"),
        "a cache that stored this must not hand it to a client that cannot \
         decode it"
    );
    assert_eq!(
        ungzip(&encoded.body),
        plain.body,
        "both encodings carry the same playlist"
    );
    assert!(
        encoded.body.len() < plain.body.len(),
        "repetitive playlist text is what the encoding is for"
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_client_refusing_gzip_is_answered_in_the_encoding_it_asked_for() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    // `q=0` is a refusal, and it outranks the wildcard beside it.
    let refused = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "*, gzip;q=0")],
    )
    .await;
    let wildcard = request(
        harness.address,
        "GET",
        "/live/camera/0/video.m3u8",
        &[("Accept-Encoding", "*")],
    )
    .await;

    assert_eq!(refused.header("content-encoding"), None);
    assert_eq!(wildcard.header("content-encoding"), Some("gzip"));

    harness.stop().await;
}

#[tokio::test]
async fn already_compressed_media_is_neither_gzipped_nor_negotiated() {
    let harness = Harness::start().await;
    let lease = lease(&harness.store, vec![video(0)]);
    write(&lease, initialization(0, 1));
    write_segment(&lease, 0, 0, 0);

    let reply = request(
        harness.address,
        "GET",
        "/live/camera/0/segment/1.m4s",
        &[("Accept-Encoding", "gzip")],
    )
    .await;

    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-encoding"), None);
    assert_eq!(
        reply.joined("vary"),
        None,
        "announcing negotiation on a resource that never varies would split \
         every downstream cache entry for the six target durations this origin \
         asks caches to hold it"
    );
    assert_eq!(reply.body.len(), 6 * PART_BYTES);

    harness.stop().await;
}

#[tokio::test]
async fn a_multivariant_playlist_names_its_renditions() {
    let harness = Harness::start().await;
    let _lease = lease(&harness.store, vec![video(0)]);

    let reply = request(harness.address, "GET", "/live/camera/index.m3u8", &[]).await;

    assert_eq!(reply.status, 200);
    let body = String::from_utf8(reply.body).expect("a playlist is text");
    assert!(body.contains("#EXT-X-STREAM-INF:"));
    assert!(body.contains("\n0/video.m3u8\n"));

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
        Some("public, max-age=36, immutable")
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

    // Absence is worth remembering, and a directive naming it is worth
    // remembering longer; a malformed request is the client's to fix and is
    // never cached for anyone else.
    for (target, expected, caching) in [
        ("/nobody/here/index.m3u8", 404, "public, max-age=6"),
        ("/live/camera/9/video.m3u8", 404, "public, max-age=6"),
        ("/live/camera/0/segment/999.m4s", 404, "public, max-age=6"),
        (
            "/live/camera/9/video.m3u8?_HLS_msn=1",
            404,
            "public, max-age=24",
        ),
        ("/live/camera/0/video.m3u8?_HLS_part=2", 400, "no-cache"),
        ("/live/camera/0/video.m3u8?_HLS_msn=9999", 400, "no-cache"),
    ] {
        let reply = request(harness.address, "GET", target, &[]).await;
        assert_eq!(reply.status, expected, "{target}");
        assert_eq!(reply.header("cache-control"), Some(caching), "{target}");
    }

    let rejected = request(harness.address, "POST", "/live/camera/index.m3u8", &[]).await;
    assert_eq!(rejected.status, 405);
    // OPTIONS is advertised because the origin genuinely answers preflights.
    assert_eq!(rejected.header("allow"), Some("GET, HEAD, OPTIONS"));

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
            "/live/camera/0/video.m3u8?_HLS_msn=1&_HLS_part=1",
            &[],
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!held.is_finished(), "the part has not been published yet");

    write(&lease, chunk(0, 1, 1, 7));
    let reply = held.await.expect("the request task ran");

    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.header("cache-control"),
        Some("public, max-age=36, no-transform"),
        "the directive is part of the URL, so these bytes answer one exact \
         playlist state and can never become the wrong answer to it"
    );
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
        "/live/camera/0/video.m3u8?_HLS_msn=2",
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
    assert_eq!(
        reply.header("cache-control"),
        Some("no-cache"),
        "coming back is pointless if a cache answers every retry with the \
         same failure"
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
            "/live/camera/0/video.m3u8?_HLS_msn=1&_HLS_part=1",
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

mod end_to_end {
    use std::{
        fs,
        io::Cursor,
        path::Path,
        process::Command,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use crate::{
        admission::{
            ClientInfo, IngestProtocol, OpenStreamAuthenticator, PresentedCredential, PublishGrant,
            PublishRequest, PublishResource, StreamPolicy,
        },
        delivery::hls::service::PlaylistReadiness,
        domain::BoxFuture,
        observe::{Events, SourceMeters},
        segment::SegmentationPolicy,
        server::{Node, NodeConfig},
        session::{PendingPermit, SessionOutcome, run_session},
        source::{
            AcceptedPublish, PendingPublish, PublishRejection, TransportError,
            avformat::{AvformatConfig, AvformatPacketSource, ReadInput},
        },
    };

    use super::{HttpConfig, Readiness, TcpListener, request, serve};

    struct FixturePublish {
        request: PublishRequest,
        bytes: Vec<u8>,
        config: AvformatConfig,
        input: crate::source::InputLimits,
    }

    impl PendingPublish for FixturePublish {
        fn publish_request(&self) -> Result<PublishRequest, TransportError> {
            Ok(self.request.clone())
        }

        fn accept(
            self: Box<Self>,
            grant: PublishGrant,
            meters: Arc<dyn SourceMeters>,
        ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
            Box::pin(async move {
                let source = AvformatPacketSource::new(
                    Box::new(ReadInput::closed(Cursor::new(self.bytes))),
                    self.config,
                    self.input,
                    meters,
                )
                .map_err(|error| TransportError::Accept(error.to_string().into()))?;
                Ok(AcceptedPublish {
                    source: Box::new(source),
                    grant,
                })
            })
        }

        fn reject(
            self: Box<Self>,
            _rejection: PublishRejection,
        ) -> BoxFuture<'static, Result<(), TransportError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn publish(input: crate::source::InputLimits) -> Box<dyn PendingPublish> {
        publish_bytes(input, repeated_aac_flv(20))
    }

    fn publish_bytes(input: crate::source::InputLimits, bytes: Vec<u8>) -> Box<dyn PendingPublish> {
        Box::new(FixturePublish {
            request: PublishRequest {
                protocol: IngestProtocol::Rtmp,
                resource: PublishResource {
                    namespace: Some("live".into()),
                    name: "camera".into(),
                },
                credential: PresentedCredential::new("secret"),
                client: ClientInfo {
                    remote_address: "127.0.0.1:1935".parse().expect("constant is valid"),
                    encoder: Some("checked-in fixture".into()),
                    protocol_version: None,
                },
            },
            bytes,
            config: AvformatConfig::default(),
            input,
        })
    }

    fn node() -> (Node, crate::session::SessionConfig) {
        node_with_policy(StreamPolicy::permissive())
    }

    fn node_with_policy(policy: StreamPolicy) -> (Node, crate::session::SessionConfig) {
        let mut config = NodeConfig::default();
        // Eight AAC units per segment and four per part keep the fixture small
        // while giving pre-roll enough cadence evidence for both boundaries.
        config.session.segmentation = SegmentationPolicy::latency_first(
            Duration::from_millis(180),
            Duration::from_millis(90),
        );
        config.hls.readiness = PlaylistReadiness::CompletedSegment;
        let session = config.session;
        let node = Node::new(
            config,
            Arc::new(OpenStreamAuthenticator::new(policy)),
            Events::default(),
        )
        .expect("node configuration is valid");
        (node, session)
    }

    #[tokio::test]
    async fn avformat_through_normalization_cmaf_hls_and_http_is_playable() {
        let (node, session) = node();
        let outcome = run_session(
            publish(session.input),
            node.services(),
            &session,
            PendingPermit::unlimited(),
        )
        .await;
        assert_eq!(outcome, Ok(SessionOutcome::Ended));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral HTTP listener binds");
        let address = listener.local_addr().expect("listener has an address");
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            node.application(),
            HttpConfig::default(),
            None,
            Readiness::ready(),
            async {
                let _ = stopped.await;
            },
        ));

        let multivariant = request(address, "GET", "/live/camera/index.m3u8", &[]).await;
        assert_eq!(multivariant.status, 200);
        assert!(
            String::from_utf8(multivariant.body)
                .expect("the multivariant playlist is text")
                // The fixture publishes audio alone, and the playlist name says
                // so without anyone having to resolve rendition 0 first.
                .contains("\n0/audio.m3u8\n")
        );
        let media = request(address, "GET", "/live/camera/0/audio.m3u8", &[]).await;
        assert_eq!(media.status, 200);
        let media = String::from_utf8(media.body).expect("media playlist is text");
        assert!(media.contains("#EXT-X-MAP:"));
        assert!(media.contains("segment/"));
        let initialization = media
            .lines()
            .find_map(|line| {
                line.strip_prefix("#EXT-X-MAP:URI=\"")
                    .and_then(|value| value.strip_suffix('"'))
            })
            .expect("media playlist names its initialization");
        let segment = media
            .lines()
            .find(|line| {
                !line.starts_with('#')
                    && Path::new(line)
                        .extension()
                        .is_some_and(|ext: &std::ffi::OsStr| ext.eq_ignore_ascii_case("m4s"))
            })
            .expect("media playlist names a segment");
        let initialization = request(
            address,
            "GET",
            &format!("/live/camera/0/{initialization}"),
            &[],
        )
        .await;
        let segment = request(address, "GET", &format!("/live/camera/0/{segment}"), &[]).await;
        assert_eq!((initialization.status, segment.status), (200, 200));
        validate_with_ffprobe(&initialization.body, &segment.body);

        if command_exists("mediastreamvalidator") {
            let url = format!("http://{address}/live/camera/index.m3u8");
            let report = temporary_path("json");
            let validation = Command::new("mediastreamvalidator")
                .args(["--timeout", "10", "--validation-data-path"])
                .arg(&report)
                .arg(&url)
                .output()
                .expect("Apple validator starts");
            if report.exists() {
                fs::remove_file(&report).expect("Apple validation report is removed");
            }
            assert!(
                validation.status.success(),
                "Apple playlist validation failed:\n{}\n{}",
                String::from_utf8_lossy(&validation.stdout),
                String::from_utf8_lossy(&validation.stderr)
            );
        }

        let _ = shutdown.send(());
        server
            .await
            .expect("HTTP task did not panic")
            .expect("HTTP server stopped cleanly");
    }

    #[tokio::test]
    async fn flv_script_data_captions_become_a_webvtt_rendition() {
        // Both names FFmpeg extracts through the same path must produce the
        // same rendition, so the whole publication runs once for each.
        for name in [b"onTextData".as_slice(), b"onCaption".as_slice()] {
            let body = published_captions(name).await;
            let publisher = String::from_utf8_lossy(name);
            assert!(
                body.contains("third caption"),
                "{publisher} cue text survives:\n{body}"
            );
            // A cue may repeat across segments — delivery serves overlapping
            // windows — but two different cues may never be on screen at once,
            // which is what an unresolved open-ended cue would produce.
            assert!(
                !overlapping_cues(&body),
                "{publisher} produced overlapping cues:\n{body}"
            );
        }
    }

    /// Publishes a captioned FLV and returns its whole WebVTT rendition.
    async fn published_captions(name: &[u8]) -> String {
        let mut policy = StreamPolicy::permissive();
        // Bare cue text arrives from FLV script data, which the permissive
        // set does not name.
        policy.subtitles.codecs = crate::admission::Codecs::OneOf(vec![
            crate::domain::Codec::WebVtt,
            crate::domain::Codec::SubRip,
            crate::domain::Codec::Text,
        ]);
        let (node, session) = node_with_policy(policy);
        let cues: [(u32, &[u8]); 3] = [
            (40, b"first caption"),
            (120, b"second caption"),
            (200, b"third caption"),
        ];
        let outcome = run_session(
            publish_bytes(session.input, captioned_aac_flv(20, name, &cues)),
            node.services(),
            &session,
            PendingPermit::unlimited(),
        )
        .await;
        assert_eq!(outcome, Ok(SessionOutcome::Ended));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral HTTP listener binds");
        let address = listener.local_addr().expect("listener has an address");
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            node.application(),
            HttpConfig::default(),
            None,
            Readiness::ready(),
            async {
                let _ = stopped.await;
            },
        ));

        let multivariant = request(address, "GET", "/live/camera/index.m3u8", &[]).await;
        assert_eq!(multivariant.status, 200);
        let multivariant =
            String::from_utf8(multivariant.body).expect("the multivariant playlist is text");
        let subtitles = multivariant
            .lines()
            .filter(|line| line.contains("TYPE=SUBTITLES"))
            .find_map(|line| {
                line.split_once("URI=\"")
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(uri, _)| uri.to_owned())
            })
            .expect("captions are declared as a subtitle rendition");

        let media = request(address, "GET", &format!("/live/camera/{subtitles}"), &[]).await;
        assert_eq!(media.status, 200);
        let media = String::from_utf8(media.body).expect("media playlist is text");

        // The WEBVTT header is delivered once through EXT-X-MAP, so segment
        // bodies carry cues alone.
        let initialization = media
            .lines()
            .find_map(|line| {
                line.strip_prefix("#EXT-X-MAP:URI=\"")
                    .and_then(|value| value.strip_suffix('"'))
            })
            .expect("the subtitle playlist names its initialization");
        let header = fetch(address, &relative_to(&subtitles, initialization)).await;
        assert!(header.starts_with("WEBVTT"), "initialization:\n{header}");

        // Concatenated across the rendition: this cadence is far shorter than
        // a cue, so no single segment holds the whole caption sequence.
        let mut body = String::new();
        for segment in media
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        {
            body.push_str(&fetch(address, &relative_to(&subtitles, segment)).await);
        }

        let _ = shutdown.send(());
        server
            .await
            .expect("HTTP task did not panic")
            .expect("HTTP server stopped cleanly");
        body
    }

    /// Fetches one rendition-relative path, requiring a 200.
    async fn fetch(address: std::net::SocketAddr, path: &str) -> String {
        let response = request(address, "GET", &format!("/live/camera/{path}"), &[]).await;
        assert_eq!(response.status, 200, "GET {path}");
        String::from_utf8(response.body).expect("WebVTT resources are text")
    }

    /// Resolves a URI named relative to its media playlist.
    fn relative_to(playlist: &str, uri: &str) -> String {
        match playlist.rsplit_once('/') {
            Some((directory, _)) => format!("{directory}/{uri}"),
            None => uri.to_owned(),
        }
    }

    /// Whether two distinct cues are ever on screen at the same time.
    ///
    /// A cue repeated verbatim across segments is ordinary — delivery serves
    /// overlapping windows — so identical spans are not an overlap. Two cues
    /// with *different* spans that intersect are, because a player would show
    /// both at once.
    fn overlapping_cues(body: &str) -> bool {
        let mut spans: Vec<(u64, u64)> = body
            .lines()
            .filter_map(|line| line.split_once(" --> "))
            .filter_map(|(start, end)| Some((timestamp_ms(start)?, timestamp_ms(end.trim())?)))
            .collect();
        spans.sort_unstable();
        spans.dedup();
        spans
            .windows(2)
            .any(|pair| pair[1].0 < pair[0].1 || pair[0].1 <= pair[0].0)
    }

    /// `HH:MM:SS.mmm` as milliseconds.
    fn timestamp_ms(value: &str) -> Option<u64> {
        let (hours, rest) = value.trim().split_once(':')?;
        let (minutes, rest) = rest.split_once(':')?;
        let (seconds, milliseconds) = rest.split_once('.')?;
        Some(
            hours.parse::<u64>().ok()? * 3_600_000
                + minutes.parse::<u64>().ok()? * 60_000
                + seconds.parse::<u64>().ok()? * 1_000
                + milliseconds.parse::<u64>().ok()?,
        )
    }

    fn repeated_aac_flv(frames: usize) -> Vec<u8> {
        use crate::mux::fixtures::{AAC_EXTRADATA, AAC_FRAME, AAC_FRAME_SAMPLES};

        let mut flv = b"FLV\x01\x04\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        let mut sequence = Vec::with_capacity(2 + AAC_EXTRADATA.len());
        sequence.extend_from_slice(&[0xaf, 0]);
        sequence.extend_from_slice(AAC_EXTRADATA);
        push_flv_tag(&mut flv, 8, 0, &sequence);
        for frame in 0..frames {
            let mut payload = Vec::with_capacity(2 + AAC_FRAME.len());
            payload.extend_from_slice(&[0xaf, 1]);
            payload.extend_from_slice(AAC_FRAME);
            let timestamp = u32::try_from(frame as u64 * AAC_FRAME_SAMPLES * 1_000 / 48_000)
                .expect("test fixture timestamp fits");
            push_flv_tag(&mut flv, 8, timestamp, &payload);
        }
        flv
    }

    fn push_flv_tag(output: &mut Vec<u8>, kind: u8, timestamp: u32, payload: &[u8]) {
        let length = u32::try_from(payload.len()).expect("test payload fits");
        output.push(kind);
        output.extend_from_slice(&u24_be(length));
        output.extend_from_slice(&u24_be(timestamp));
        output.push(u8::try_from(timestamp >> 24).unwrap_or(0));
        output.extend_from_slice(&[0, 0, 0]);
        output.extend_from_slice(payload);
        output.extend_from_slice(&(11 + length).to_be_bytes());
    }

    /// AAC audio interleaved with FLV script-data captions under `name`.
    ///
    /// `onTextData` and `onCaption` are the two names FFmpeg's FLV demuxer
    /// extracts through the same path, so a fixture parameterized on the name
    /// is what shows they produce the same rendition rather than asserting it.
    fn captioned_aac_flv(frames: usize, name: &[u8], cues: &[(u32, &[u8])]) -> Vec<u8> {
        let mut flv = repeated_aac_flv(frames);
        let mut tagged = flv.split_off(13);
        let mut out = flv;

        // Script tags are interleaved by timestamp, as a publisher sends them.
        let mut emitted = 0;
        let mut cursor = 0;
        while cursor + 11 <= tagged.len() {
            let size = usize::from(tagged[cursor + 1]) << 16
                | usize::from(tagged[cursor + 2]) << 8
                | usize::from(tagged[cursor + 3]);
            let timestamp = u32::from(tagged[cursor + 4]) << 16
                | u32::from(tagged[cursor + 5]) << 8
                | u32::from(tagged[cursor + 6]);
            while emitted < cues.len() && cues[emitted].0 <= timestamp {
                push_flv_tag(
                    &mut out,
                    18,
                    cues[emitted].0,
                    &script_data(name, cues[emitted].1),
                );
                emitted += 1;
            }
            let end = cursor + 11 + size + 4;
            out.extend_from_slice(&tagged[cursor..end]);
            cursor = end;
        }
        while emitted < cues.len() {
            push_flv_tag(
                &mut out,
                18,
                cues[emitted].0,
                &script_data(name, cues[emitted].1),
            );
            emitted += 1;
        }
        tagged.clear();
        out
    }

    /// One AMF0 script-data body: the message name, then `{text: ...}`.
    fn script_data(name: &[u8], text: &[u8]) -> Vec<u8> {
        let mut payload = vec![0x02];
        payload.extend_from_slice(&u16::try_from(name.len()).expect("name fits").to_be_bytes());
        payload.extend_from_slice(name);
        // ECMA array with one property, which is what encoders emit.
        payload.push(0x08);
        payload.extend_from_slice(&1_u32.to_be_bytes());
        payload.extend_from_slice(&4_u16.to_be_bytes());
        payload.extend_from_slice(b"text");
        payload.push(0x02);
        payload.extend_from_slice(&u16::try_from(text.len()).expect("text fits").to_be_bytes());
        payload.extend_from_slice(text);
        payload.extend_from_slice(&[0x00, 0x00, 0x09]);
        payload
    }

    fn u24_be(value: u32) -> [u8; 3] {
        let bytes = value.to_be_bytes();
        [bytes[1], bytes[2], bytes[3]]
    }

    fn validate_with_ffprobe(initialization: &[u8], segment: &[u8]) {
        if !command_exists("ffprobe") {
            return;
        }
        let path = temporary_path("mp4");
        let mut media = Vec::with_capacity(initialization.len() + segment.len());
        media.extend_from_slice(initialization);
        media.extend_from_slice(segment);
        fs::write(&path, media).expect("temporary CMAF presentation is written");
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-show_streams", "-of", "json"])
            .arg(&path)
            .output()
            .expect("ffprobe starts");
        fs::remove_file(&path).expect("temporary CMAF presentation is removed");

        assert!(
            probe.status.success(),
            "ffprobe rejected packaged media: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let report = String::from_utf8(probe.stdout).expect("ffprobe JSON is UTF-8");
        assert!(report.contains("\"codec_name\": \"aac\""));
        assert!(report.contains("\"time_base\": \"1/48000\""));
    }

    fn command_exists(program: &str) -> bool {
        Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn temporary_path(extension: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time follows the epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rushls-e2e-{}-{nonce}.{extension}",
            std::process::id()
        ))
    }
}

#[tokio::test]
async fn a_preflight_is_answered_without_reaching_the_origin() {
    let harness = Harness::start().await;

    let reply = request(
        harness.address,
        "OPTIONS",
        "/live/camera/index.m3u8",
        &[
            ("Origin", "https://player.example"),
            ("Access-Control-Request-Method", "GET"),
            ("Access-Control-Request-Headers", "range"),
        ],
    )
    .await;

    // Answered rather than producing the 404 this unknown stream would
    // otherwise give: the browser asked what it may send, not for any media.
    // Any 2xx satisfies a preflight; the middleware uses 200 where this origin
    // used to send 204.
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("access-control-allow-origin"), Some("*"));
    assert_eq!(
        reply.header("access-control-allow-methods"),
        Some("GET,HEAD,OPTIONS")
    );
    assert_eq!(reply.header("access-control-allow-headers"), Some("range"));
    assert!(reply.body.is_empty());

    harness.stop().await;
}

#[tokio::test]
async fn an_allowlisted_origin_is_echoed_and_the_response_says_it_varies() {
    let harness = Harness::start_with(HttpConfig {
        cors: CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec![
                OriginPattern::parse("https://player.example").expect("the pattern is valid"),
            ]),
            ..CorsConfig::default()
        },
        ..HttpConfig::default()
    })
    .await;
    let _lease = lease(&harness.store, vec![video(0)]);

    let allowed = request(
        harness.address,
        "GET",
        "/live/camera/index.m3u8",
        &[("Origin", "https://player.example")],
    )
    .await;
    assert_eq!(allowed.status, 200);
    assert_eq!(
        allowed.header("access-control-allow-origin"),
        Some("https://player.example")
    );
    // Without Origin here, a CDN would hand this viewer's allowed origin to
    // every other viewer, and playback would break for all of them. A playlist
    // is also content-negotiated, so both fields have to be listed.
    assert_eq!(
        allowed.joined("vary").as_deref(),
        Some("Accept-Encoding, origin")
    );

    let refused = request(
        harness.address,
        "GET",
        "/live/camera/index.m3u8",
        &[("Origin", "https://elsewhere.test")],
    )
    .await;
    assert_eq!(refused.status, 200, "the media is not itself restricted");
    assert_eq!(refused.header("access-control-allow-origin"), None);
    assert_eq!(
        refused.joined("vary").as_deref(),
        Some("Accept-Encoding, origin")
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_wildcard_allowlist_admits_subdomains_and_refuses_lookalikes() {
    let harness = Harness::start_with(HttpConfig {
        cors: CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec![
                OriginPattern::parse("https://*.example.com").expect("the pattern is valid"),
            ]),
            ..CorsConfig::default()
        },
        ..HttpConfig::default()
    })
    .await;
    let _lease = lease(&harness.store, vec![video(0)]);

    let allowed = cors_probe(&harness, "https://player.example.com").await;
    assert_eq!(allowed, Some("https://player.example.com".to_owned()));

    // Registrable by anyone; the naive suffix match would hand it the stream.
    assert_eq!(cors_probe(&harness, "https://evil-example.com").await, None);
    // The apex is not a subdomain of itself.
    assert_eq!(cors_probe(&harness, "https://example.com").await, None);
    // One label only, so a deeper name needs its own entry.
    assert_eq!(cors_probe(&harness, "https://a.b.example.com").await, None);
    // Scheme and port are part of the origin.
    assert_eq!(
        cors_probe(&harness, "http://player.example.com").await,
        None
    );
    assert_eq!(
        cors_probe(&harness, "https://player.example.com:8443").await,
        None
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_double_wildcard_admits_any_depth_but_still_not_the_apex() {
    let harness = Harness::start_with(HttpConfig {
        cors: CorsConfig {
            allowed_origins: AllowedOrigins::Only(vec![
                OriginPattern::parse("https://**.video.example.com").expect("the pattern is valid"),
            ]),
            ..CorsConfig::default()
        },
        ..HttpConfig::default()
    })
    .await;
    let _lease = lease(&harness.store, vec![video(0)]);

    for origin in [
        "https://a.video.example.com",
        "https://b.c.video.example.com",
    ] {
        assert_eq!(
            cors_probe(&harness, origin).await,
            Some(origin.to_owned()),
            "{origin}"
        );
    }
    assert_eq!(
        cors_probe(&harness, "https://video.example.com").await,
        None
    );
    assert_eq!(
        cors_probe(&harness, "https://video.example.com.evil.test").await,
        None
    );

    harness.stop().await;
}

/// The `Access-Control-Allow-Origin` a real request gets back, if any.
///
/// Asserted over the wire rather than against the matcher, because what
/// matters is the header a browser would actually receive.
async fn cors_probe(harness: &Harness, origin: &str) -> Option<String> {
    let reply = request(
        harness.address,
        "GET",
        "/live/camera/index.m3u8",
        &[("Origin", origin)],
    )
    .await;
    assert_eq!(reply.status, 200, "the media itself is never restricted");
    // Carried alongside whatever content negotiation already set — gzip adds
    // `Accept-Encoding` — rather than replacing it, which would let a cache
    // serve a compressed body to a client that never asked for one.
    let vary = reply.joined("vary").expect("an allowlisted policy varies");
    assert!(
        vary.split(',')
            .any(|field| field.trim().eq_ignore_ascii_case("origin")),
        "a CDN must keep the answers apart, got `{vary}`"
    );
    reply
        .header("access-control-allow-origin")
        .map(str::to_owned)
}

/// TLS termination, ALPN, and certificate rotation without a restart.
///
/// Driven with a real rustls client rather than by inspecting the resolver,
/// because the claim being tested is about what a peer sees on the wire: which
/// certificate it is handed, and that a rotation changes the answer for new
/// connections while the origin keeps serving throughout.
mod tls {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use parking_lot::Mutex;
    use rustls::{
        ClientConfig, DigitallySignedStruct, SignatureScheme,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
        pki_types::{CertificateDer, ServerName, UnixTime},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };
    use tokio_rustls::TlsConnector;

    use crate::{
        delivery::hls::{
            StreamStore,
            fixtures::{lease, video},
        },
        observe::{NodeEvent, ProcessMeters},
        server::http::fixtures::{
            NodeEventRecorder, application, scratch, write_atomically, write_pair,
            write_projected_pair,
        },
    };

    use super::super::{HttpConfig, Readiness, TlsSettings, bind_tls, serve};
    use tokio::net::TcpListener;

    /// Accepts any certificate and remembers what it was handed.
    ///
    /// The alternative — trusting the generated certificate as a root — does
    /// not work for a self-signed leaf, and would in any case answer a
    /// different question than the one these tests ask.
    #[derive(Debug)]
    struct Capturing {
        presented: Mutex<Vec<Vec<u8>>>,
        provider: Arc<CryptoProvider>,
    }

    impl ServerCertVerifier for Capturing {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            self.presented.lock().push(end_entity.to_vec());
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// A TLS origin, plus the recorder its process events land in.
    struct TlsHarness {
        address: std::net::SocketAddr,
        store: StreamStore,
        events: Arc<NodeEventRecorder>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        served: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    }

    impl TlsHarness {
        async fn start(settings: TlsSettings) -> Self {
            let store = StreamStore::default();
            let origin = application(&store);
            let tcp = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("an ephemeral port is available");
            let address = tcp.local_addr().expect("the listener is bound");
            let (recorder, events) = NodeEventRecorder::install();
            let listener = bind_tls(tcp, settings, ProcessMeters::default(), events)
                .expect("the certificate loads");
            let config = HttpConfig::default();
            let (shutdown, signal) = tokio::sync::oneshot::channel();
            let served = tokio::spawn(serve(
                listener,
                origin,
                config,
                None,
                Readiness::ready(),
                async {
                    let _ = signal.await;
                },
            ));
            Self {
                address,
                store,
                events: recorder,
                shutdown: Some(shutdown),
                served: Some(served),
            }
        }

        /// Handshakes once, returning the certificate presented and the
        /// protocol ALPN settled on.
        async fn handshake(&self, offer: &[&[u8]]) -> (Vec<u8>, Option<Vec<u8>>) {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let verifier = Arc::new(Capturing {
                presented: Mutex::new(Vec::new()),
                provider: Arc::clone(&provider),
            });
            let mut config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the default versions are supported")
                .dangerous()
                .with_custom_certificate_verifier(
                    Arc::clone(&verifier) as Arc<dyn ServerCertVerifier>
                )
                .with_no_client_auth();
            config.alpn_protocols = offer.iter().map(|name| name.to_vec()).collect();

            let stream = TcpStream::connect(self.address)
                .await
                .expect("the origin is up");
            let name = ServerName::try_from("origin.test").expect("a valid name");
            let stream = TlsConnector::from(Arc::new(config))
                .connect(name, stream)
                .await
                .expect("the handshake completes");
            let alpn = stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
            let presented = verifier
                .presented
                .lock()
                .first()
                .cloned()
                .expect("the server presented a certificate");
            (presented, alpn)
        }

        /// Handshakes until the expected certificate shows up, or gives up.
        ///
        /// Polling rather than sleeping a fixed interval: the reload is gated
        /// on a filesystem notification and a debounce, and pinning the test to
        /// a guessed duration would make it flaky on a loaded machine without
        /// making it any stricter.
        async fn await_certificate(&self, expected: &[u8]) -> bool {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if self.handshake(&[b"http/1.1"]).await.0 == expected {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
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

    /// One HTTPS GET, spoken by hand over the TLS stream.
    ///
    /// HTTP/1.1 is offered alone so the reply is the same wire format the
    /// cleartext tests parse; that ALPN can also settle on h2 is asserted
    /// separately.
    async fn https_get(harness: &TlsHarness, target: &str) -> String {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(Capturing {
            presented: Mutex::new(Vec::new()),
            provider: Arc::clone(&provider),
        });
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("the default versions are supported")
            .dangerous()
            .with_custom_certificate_verifier(verifier as Arc<dyn ServerCertVerifier>)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let stream = TcpStream::connect(harness.address)
            .await
            .expect("the origin is up");
        let name = ServerName::try_from("origin.test").expect("a valid name");
        let mut stream = TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await
            .expect("the handshake completes");
        stream
            .write_all(
                format!("GET {target} HTTP/1.1\r\nHost: origin\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("the request is sent");
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .expect("the response completes");
        String::from_utf8_lossy(&raw).into_owned()
    }

    fn loaded_certificates(harness: &TlsHarness) -> usize {
        harness
            .events
            .recorded()
            .iter()
            .filter(|event| matches!(event, NodeEvent::CertificateLoaded { .. }))
            .count()
    }

    #[tokio::test]
    async fn a_playlist_is_served_over_tls_and_alpn_offers_http_2() {
        let directory = scratch("serves");
        let (settings, _) = write_pair(&directory, "origin.test");
        let harness = TlsHarness::start(settings).await;
        let _lease = lease(&harness.store, vec![video(0)]);

        let response = https_get(&harness, "/live/camera/index.m3u8").await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected a playlist, got: {response}"
        );
        assert!(response.contains("#EXT-X-STREAM-INF:"));

        // The reason to terminate TLS in-process at all: a browser reaches
        // HTTP/2 only this way, and the Low-Latency profile expects it.
        let (_, alpn) = harness.handshake(&[b"h2", b"http/1.1"]).await;
        assert_eq!(alpn.as_deref(), Some(b"h2".as_slice()));

        harness.stop().await;
    }

    #[tokio::test]
    async fn a_rotated_certificate_is_presented_without_a_restart() {
        let directory = scratch("rotates");
        let (settings, first) = write_pair(&directory, "origin.test");
        let harness = TlsHarness::start(settings.clone()).await;

        assert_eq!(harness.handshake(&[b"http/1.1"]).await.0, first);

        let (_, second) = write_pair(&directory, "origin.test");
        assert_ne!(first, second, "the rotation produced a new certificate");

        assert!(
            harness.await_certificate(&second).await,
            "the rotated certificate was never presented"
        );
        // Once at startup and once for the rotation: the event stream alone
        // answers which certificate this process is serving.
        assert_eq!(loaded_certificates(&harness), 2);

        harness.stop().await;
    }

    /// The deployment this feature exists for.
    ///
    /// A Kubernetes secret mount rotates by swapping the `..data` symlink, so
    /// no filesystem event ever names the configured `tls.crt`. Filtering
    /// events by path — the obvious implementation — silently never reloads
    /// here while passing every test that writes files directly.
    #[tokio::test]
    async fn a_projected_secret_rotates_even_though_no_event_names_the_certificate() {
        let directory = scratch("projected");
        let (settings, first) = write_projected_pair(&directory, "origin.test");
        let harness = TlsHarness::start(settings).await;

        assert_eq!(harness.handshake(&[b"http/1.1"]).await.0, first);

        let (_, second) = write_projected_pair(&directory, "origin.test");
        assert_ne!(first, second, "the rotation produced a new certificate");

        assert!(
            harness.await_certificate(&second).await,
            "a secret-mount rotation was never picked up"
        );

        harness.stop().await;
    }

    #[tokio::test]
    async fn a_broken_rotation_leaves_the_previous_certificate_serving() {
        let directory = scratch("broken");
        let (settings, first) = write_pair(&directory, "origin.test");
        let harness = TlsHarness::start(settings.clone()).await;

        // A key that belongs to some other certificate: the shape a rotation
        // takes when only one of the two files has landed.
        let elsewhere = scratch("broken-source");
        let (foreign, _) = write_pair(&elsewhere, "other.test");
        let key = std::fs::read(&foreign.key).expect("the foreign key is readable");
        write_atomically(&settings.key, &key);

        let rejected = await_rejection(&harness).await;
        assert!(rejected, "the mismatched pair was never reported");
        assert_eq!(
            harness.handshake(&[b"http/1.1"]).await.0,
            first,
            "a rejected rotation must not disturb what is being served"
        );
        assert_eq!(loaded_certificates(&harness), 1);

        harness.stop().await;
    }

    async fn await_rejection(harness: &TlsHarness) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if harness
                .events
                .recorded()
                .iter()
                .any(|event| matches!(event, NodeEvent::CertificateRejected { .. }))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_missing_certificate_fails_the_bind_rather_than_serving_cleartext() {
        let directory = scratch("missing");
        let settings = TlsSettings {
            certificate: directory.join("absent.pem"),
            key: directory.join("absent.key"),
            ..TlsSettings::default()
        };
        let tcp = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port is available");
        let (_, events) = NodeEventRecorder::install();

        assert!(bind_tls(tcp, settings, ProcessMeters::default(), events).is_err());
    }

    #[test]
    fn hostile_range_headers_never_panic() {
        use crate::server::http::body::parse_range;

        let mut state = 0xa55a_5aa5_1234_5678_u64;
        for _ in 0..4_000 {
            let header = crate::test_fuzz::string(&mut state, 48);
            let length = crate::test_fuzz::next_random(&mut state) & 0x1_0000;
            // Every outcome is legal: served, ignored, or unsatisfiable.
            let _ = parse_range(&header, length);
        }
    }
}
