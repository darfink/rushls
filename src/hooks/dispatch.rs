//! Draining one hook's queue, one request per stream at a time.

use std::{sync::Arc, time::Duration};

use http::StatusCode;
use tokio::{sync::watch, task::JoinSet};

use crate::{
    domain::StreamId,
    observe::{Events, NodeEvent},
    outbound::{HttpClient, OutboundError, Response},
};

use super::{CONTENT_TYPE, Dispatchers, Envelope, Loss, Shared};

/// First wait between attempts, doubling from there.
///
/// Not configurable: the shape of a backoff curve is an implementation detail,
/// while how long an operator is willing to keep trying is `maximum_attempts`.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAXIMUM_BACKOFF: Duration = Duration::from_secs(30);

impl Dispatchers {
    /// Runs every configured hook until `stop`, then drains what it can.
    pub async fn run(self, mut stop: watch::Receiver<bool>) {
        let mut hooks = JoinSet::new();
        for shared in self.shared {
            hooks.spawn(run_hook(
                shared,
                self.client.clone(),
                self.drain_timeout,
                self.events.clone(),
                stop.clone(),
            ));
        }
        if hooks.is_empty() {
            // Nothing configured: wait for shutdown rather than returning, so a
            // caller that joins this task is not woken immediately.
            let _ = stop.wait_for(|stopped| *stopped).await;
            return;
        }
        while hooks.join_next().await.is_some() {}
    }
}

async fn run_hook(
    shared: Arc<Shared>,
    client: HttpClient,
    drain_timeout: Duration,
    events: Events,
    mut stop: watch::Receiver<bool>,
) {
    let mut sending = JoinSet::new();

    loop {
        start_available(&shared, &client, &events, &mut sending);

        tokio::select! {
            biased;

            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }

            Some(finished) = sending.join_next(), if !sending.is_empty() => {
                release(&shared, finished.ok());
            }

            () = shared.arrived.notified() => {}
        }
    }

    drain(shared, client, drain_timeout, events, sending).await;
}

/// Starts requests for as many idle streams as the concurrency limit allows.
fn start_available(
    shared: &Arc<Shared>,
    client: &HttpClient,
    events: &Events,
    sending: &mut JoinSet<StreamId>,
) {
    while sending.len() < shared.config.maximum_in_flight {
        // Taken under the lock, sent outside it: reporting a failure emits a
        // node event, and doing that while holding the queue would put an
        // observer's work on the path of every other stream's delivery.
        let Some(envelope) = shared.queue.lock().take_ready() else {
            break;
        };
        sending.spawn(send(
            Arc::clone(shared),
            client.clone(),
            events.clone(),
            envelope,
        ));
    }
}

/// Lets a stream's next event through once its predecessor has finished.
fn release(shared: &Arc<Shared>, subject: Option<StreamId>) {
    if let Some(subject) = subject {
        shared.queue.lock().finish(&subject);
    }
}

/// Delivers one event, retrying until it lands or the attempts run out.
///
/// Retries happen here rather than by re-queuing, so the stream's slot stays
/// held for the whole thing. That is deliberate: releasing it between attempts
/// would let the next event overtake the one being retried, which is exactly
/// the reordering the per-stream rule exists to prevent. The cost is that one
/// unreachable endpoint stalls that stream for up to
/// `maximum_attempts` × backoff.
async fn send(
    shared: Arc<Shared>,
    client: HttpClient,
    events: Events,
    envelope: Envelope,
) -> StreamId {
    let mut backoff = INITIAL_BACKOFF;

    for attempt in 1..=shared.config.maximum_attempts {
        let result = client
            .post(
                &shared.config.endpoint,
                CONTENT_TYPE,
                shared.config.bearer.as_ref(),
                envelope.body.clone(),
            )
            .await;

        match verdict(&result) {
            Verdict::Delivered => {
                shared
                    .meters
                    .delivered
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return envelope.subject;
            }
            Verdict::Rejected => {
                report(&shared, &events, &envelope, Loss::Rejected, &result);
                return envelope.subject;
            }
            Verdict::Retry if attempt == shared.config.maximum_attempts => {
                report(&shared, &events, &envelope, Loss::Exhausted, &result);
                return envelope.subject;
            }
            Verdict::Retry => {
                shared
                    .meters
                    .retried
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // An endpoint asking to be left alone knows better than the
                // backoff curve does, so its answer wins — but only up to the
                // cap, since it is also the party currently misbehaving.
                let wait = result
                    .as_ref()
                    .ok()
                    .and_then(|response| response.retry_after(MAXIMUM_BACKOFF))
                    .unwrap_or(backoff);
                tokio::time::sleep(wait).await;
                backoff = (backoff * 2).min(MAXIMUM_BACKOFF);
            }
        }
    }

    envelope.subject
}

enum Verdict {
    Delivered,
    /// Worth another attempt: the endpoint or the path to it is unwell.
    Retry,
    /// Retrying cannot help.
    Rejected,
}

fn verdict(result: &Result<Response, OutboundError>) -> Verdict {
    match result {
        Ok(response) if response.status.is_success() => Verdict::Delivered,
        Ok(response) => match response.status {
            // Transient by definition, plus the two the specifications reserve
            // for "ask again": a request timeout and too-early.
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS => {
                Verdict::Retry
            }
            status if status.is_server_error() => Verdict::Retry,
            // Any other 4xx is the consumer saying the request itself is wrong.
            // Retrying would move a poison event to the front of this stream's
            // queue for the whole backoff, delaying everything behind it.
            _ => Verdict::Rejected,
        },
        Err(
            OutboundError::Timeout | OutboundError::Unreachable(_) | OutboundError::Response(_),
        ) => Verdict::Retry,
        // An endpoint answering an event POST with a body this large is
        // misconfigured, and every retry pays to download it again.
        Err(OutboundError::ResponseTooLarge { .. }) => Verdict::Rejected,
    }
}

fn report(
    shared: &Arc<Shared>,
    events: &Events,
    envelope: &Envelope,
    loss: Loss,
    result: &Result<Response, OutboundError>,
) {
    shared.meters.record_loss(loss);
    events.emit(NodeEvent::HookEventDropped {
        hook: Arc::clone(&shared.config.name),
        event: envelope.id.clone(),
        kind: envelope.kind,
        reason: loss.as_str(),
        detail: match result {
            Ok(response) => format!("HTTP {}", response.status),
            Err(error) => error.to_string(),
        },
    });
}

/// Finishes what is in flight, then what is queued, until the deadline.
async fn drain(
    shared: Arc<Shared>,
    client: HttpClient,
    timeout: Duration,
    events: Events,
    mut sending: JoinSet<StreamId>,
) {
    let deadline = tokio::time::Instant::now() + timeout;

    let drained = tokio::time::timeout_at(deadline, async {
        loop {
            start_available(&shared, &client, &events, &mut sending);
            let Some(finished) = sending.join_next().await else {
                break;
            };
            release(&shared, finished.ok());
        }
    })
    .await;

    if drained.is_err() {
        sending.abort_all();
    }
    // Whatever is still queued is gone: it lives in memory only, and the
    // process is on its way out.
    let lost = shared.queue.lock().clear();
    for _ in 0..lost {
        shared.meters.record_loss(Loss::Shutdown);
    }
    if lost > 0 {
        events.emit(NodeEvent::HookEventsAbandoned {
            hook: Arc::clone(&shared.config.name),
            dropped: lost,
        });
    }
}
