//! Process-wide admission, including requests parked on a live edge.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use crate::observe::http::{HttpFailure, HttpMeters, HttpMethod, HttpObservation, HttpResource};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, watch},
    task::JoinSet,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpLimits {
    pub maximum_connections: usize,
    pub maximum_requests: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            maximum_connections: 4096,
            maximum_requests: 4096,
        }
    }
}

impl HttpLimits {
    pub fn validate(self) -> Result<(), &'static str> {
        if [self.maximum_connections, self.maximum_requests]
            .into_iter()
            .any(|limit| limit == 0 || limit > Semaphore::MAX_PERMITS)
        {
            return Err("HTTP connection and request limits must be positive and fit a semaphore");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct HttpBudget {
    connections: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    meters: HttpMeters,
}

impl HttpBudget {
    pub fn new(limits: HttpLimits) -> Self {
        let meters = HttpMeters::default();
        meters.0.lock().request_capacity = limits.maximum_requests;
        meters.0.lock().connection_capacity = limits.maximum_connections;
        Self {
            connections: Arc::new(Semaphore::new(limits.maximum_connections)),
            requests: Arc::new(Semaphore::new(limits.maximum_requests)),
            meters,
        }
    }
    #[must_use]
    pub fn with_meters(mut self, meters: HttpMeters) -> Self {
        {
            let mut shared = meters.0.lock();
            shared.request_capacity = self.requests.available_permits();
            shared.connection_capacity = self.connections.available_permits();
        }
        self.meters = meters;
        self
    }
}

// HLS resource paths are case-sensitive, matching the origin router.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn resource(path: &str) -> HttpResource {
    if matches!(
        path,
        "/metrics" | "/metrics/streams" | "/health/live" | "/health/ready"
    ) {
        HttpResource::Operator
    } else if path.ends_with(".m3u8") {
        HttpResource::Playlist
    } else if [".m4s", ".mp4", ".ts", ".vtt"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
    {
        HttpResource::Media
    } else {
        HttpResource::Other
    }
}

pub async fn admit_application<P: super::Application>(
    State((budget, application)): State<(HttpBudget, Arc<P>)>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(meters) = application.http_meters(request.uri().path()) {
        request.extensions_mut().insert(meters);
    }
    admit(State(budget), request, next).await
}

pub async fn admit(State(budget): State<HttpBudget>, request: Request, next: Next) -> Response {
    let method = match request.method().as_str() {
        "GET" => HttpMethod::Get,
        "HEAD" => HttpMethod::Head,
        _ => HttpMethod::Other,
    };
    let resource = resource(request.uri().path());
    let mut observations = [
        Some(budget.meters.start(resource, method)),
        request
            .extensions()
            .get::<HttpMeters>()
            .map(|meters| meters.start(resource, method)),
    ];
    let permit = budget.requests.try_acquire_owned().ok();
    let response = if permit.is_some() {
        next.run(request).await
    } else {
        budget.meters.0.lock().requests_rejected += 1;
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::RETRY_AFTER, "1"),
                (header::CACHE_CONTROL, "no-store"),
            ],
        )
            .into_response()
    };
    let reason = response
        .extensions()
        .get::<HttpFailure>()
        .copied()
        .or_else(|| match response.status() {
            StatusCode::UNAUTHORIZED => Some(HttpFailure::Unauthorized),
            StatusCode::FORBIDDEN => Some(HttpFailure::Forbidden),
            StatusCode::SERVICE_UNAVAILABLE if permit.is_none() => Some(HttpFailure::Admission),
            status if status.is_client_error() || status.is_server_error() => {
                Some(HttpFailure::Other)
            }
            _ => None,
        });
    for observation in observations.iter_mut().flatten() {
        observation.response(response.status().as_u16());
        if let Some(reason) = reason {
            observation.failure(reason);
        }
    }
    // Handler completion is not transfer completion. Keep the slot through
    // body consumption, including backpressure from a slow socket or H2 peer.
    response.map(|body| {
        if body.is_end_stream() {
            for observation in observations.iter_mut().flatten() {
                observation.finish("completed");
            }
        }
        Body::new(AdmittedBody {
            body,
            _permit: permit,
            observations,
        })
    })
}

struct AdmittedBody {
    body: Body,
    _permit: Option<OwnedSemaphorePermit>,
    observations: [Option<HttpObservation>; 2],
}

impl HttpBody for AdmittedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(bytes) = frame.data_ref() {
                    for observation in self.observations.iter().flatten() {
                        observation.bytes(bytes.len());
                    }
                }
                if self.body.is_end_stream() {
                    for observation in self.observations.iter_mut().flatten() {
                        observation.finish("completed");
                    }
                }
            }
            Poll::Ready(None) => {
                for observation in self.observations.iter_mut().flatten() {
                    observation.finish("completed");
                }
            }
            Poll::Ready(Some(Err(_))) => {
                for observation in self.observations.iter_mut().flatten() {
                    observation.finish("error");
                }
            }
            Poll::Pending => {}
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

struct ConnectionObservation(HttpMeters);
impl Drop for ConnectionObservation {
    fn drop(&mut self) {
        self.0.0.lock().connections -= 1;
    }
}

/// Serves until `shutdown`, then drains. Returns an error only when the
/// listener can no longer accept, after draining the connections it has.
pub async fn serve<L: super::HttpListener>(
    mut listener: L,
    router: Router,
    budget: HttpBudget,
    shutdown: impl Future<Output = ()> + Send,
) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    let (stop, stopped) = watch::channel(false);
    tokio::pin!(shutdown);
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let io = match accepted {
                    Ok((io, _)) => io,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                };
                // Never queue admitted sockets behind a semaphore: that would
                // turn the waiting sockets themselves into unbounded state.
                let Ok(permit) = Arc::clone(&budget.connections).try_acquire_owned() else {
                    budget.meters.0.lock().connections_rejected += 1;
                    drop(io);
                    continue;
                };
                // The HTTP parser's header timer starts after protocol detection.
                // Also bound silence and partial H2 prefaces before that point.
                let first_request = Arc::new(Notify::new());
                let ready = Arc::clone(&first_request);
                let service = TowerToHyperService::new(router.clone().layer(
                    axum::middleware::from_fn(move |request: Request, next: Next| {
                        ready.notify_one();
                        async move { next.run(request).await }
                    }),
                ));
                let mut stopped = stopped.clone();
                budget.meters.0.lock().connections += 1;
                let connection_meter = ConnectionObservation(budget.meters.clone());
                connections.spawn(async move {
                    let _observation = connection_meter;
                    let _permit = permit;
                    let mut builder = Builder::new(TokioExecutor::new());
                    builder.http1()
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(30))
                        .max_buf_size(32 * 1024);
                    builder.http2()
                        .timer(TokioTimer::new())
                        .max_header_list_size(32 * 1024)
                        .max_concurrent_streams(128)
                        .keep_alive_interval(Duration::from_secs(30))
                        .keep_alive_timeout(Duration::from_secs(10));
                    let connection = builder.serve_connection(TokioIo::new(io), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        () = async {
                            if tokio::time::timeout(Duration::from_secs(30), first_request.notified()).await.is_ok() {
                                std::future::pending::<()>().await;
                            }
                        } => {},
                        _ = stopped.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                    }
                });
            }
        }
    }
    let _ = stop.send(true);
    while connections.join_next().await.is_some() {}
    // Runtime bounds this drain. Dropping this future also drops the JoinSet,
    // aborting sockets still blocked on an unresponsive client.
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests;
