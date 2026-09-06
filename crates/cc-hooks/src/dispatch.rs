//! Draining one hook's queue, one request per subject at a time.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use http::StatusCode;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

use cc_outbound::{HttpClient, OutboundError, Response};

use crate::{
    CONTENT_TYPE, Dispatcher, Dispatchers, Envelope, HookObserver, Loss, Occurrence, Queue, Shared,
};

/// First wait between attempts, doubling from there.
///
/// Not configurable: the shape of a backoff curve is an implementation detail,
/// while how long an operator is willing to keep trying is `maximum_attempts`.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAXIMUM_BACKOFF: Duration = Duration::from_secs(30);

/// One hook's envelope type, which two type parameters would otherwise repeat.
type Item<E> = Envelope<<E as Occurrence>::Subject, <E as Occurrence>::Kind>;
type Pending<E> = Queue<<E as Occurrence>::Subject, <E as Occurrence>::Kind>;

impl<E: Occurrence> Dispatchers<E> {
    /// Runs every configured hook until `stop`, then drains what it can.
    pub async fn run(self, mut stop: watch::Receiver<bool>) {
        let mut hooks = JoinSet::new();
        for dispatcher in self.hooks {
            // A destination that configured its own identity or trust roots
            // brought its own client; everything else shares the process pool,
            // which is what keeps one TLS setup and one connection cache for
            // the ordinary case.
            let client = dispatcher
                .shared
                .config
                .client
                .clone()
                .unwrap_or_else(|| self.client.clone());
            hooks.spawn(run_hook(
                dispatcher,
                client,
                self.drain_timeout,
                Arc::clone(&self.observer),
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

async fn run_hook<E: Occurrence>(
    dispatcher: Dispatcher<E>,
    client: HttpClient,
    drain_timeout: Duration,
    observer: Arc<dyn HookObserver<E::Kind>>,
    mut stop: watch::Receiver<bool>,
) {
    let Dispatcher {
        shared,
        mut ingress,
    } = dispatcher;
    let mut queue = Pending::<E>::new(shared.config.queue_capacity);
    let mut sending = JoinSet::new();

    loop {
        drain_ingress(&shared, &mut ingress, &mut queue);
        start_available(&shared, &client, &observer, &mut queue, &mut sending);

        tokio::select! {
          biased;

          changed = stop.changed() => {
            if changed.is_err() || *stop.borrow() {
              break;
            }
          }

          Some(finished) = sending.join_next(), if !sending.is_empty() => {
            release(&shared, &mut queue, finished.ok());
          }

          Some(envelope) = ingress.recv() => {
            accept(&shared, &mut queue, envelope);
          }
        }
    }

    drain(
        shared,
        ingress,
        queue,
        client,
        drain_timeout,
        observer,
        sending,
    )
    .await;
}

/// Moves all immediately available producer work into the dispatcher-owned
/// ordering queue. Network progress is deliberately irrelevant here.
fn drain_ingress<E: Occurrence>(
    shared: &Shared<E>,
    ingress: &mut mpsc::Receiver<Item<E>>,
    queue: &mut Pending<E>,
) {
    while let Ok(envelope) = ingress.try_recv() {
        accept(shared, queue, envelope);
    }
}

fn accept<E: Occurrence>(shared: &Shared<E>, queue: &mut Pending<E>, envelope: Item<E>) {
    shared.meters.ingress_depth.fetch_sub(1, Ordering::Relaxed);
    if queue.push(envelope) {
        shared.meters.record_loss(Loss::Overflow);
    }
    shared
        .meters
        .queue_depth
        .store(queue.queued(), Ordering::Relaxed);
}

/// Starts requests for as many idle subjects as the concurrency limit allows.
fn start_available<E: Occurrence>(
    shared: &Arc<Shared<E>>,
    client: &HttpClient,
    observer: &Arc<dyn HookObserver<E::Kind>>,
    queue: &mut Pending<E>,
    sending: &mut JoinSet<E::Subject>,
) {
    while sending.len() < shared.config.maximum_in_flight {
        let Some(envelope) = queue.take_ready() else {
            break;
        };
        sending.spawn(send(
            Arc::clone(shared),
            client.clone(),
            Arc::clone(observer),
            envelope,
        ));
    }
    shared
        .meters
        .queue_depth
        .store(queue.queued(), Ordering::Relaxed);
    shared
        .meters
        .in_flight
        .store(sending.len(), Ordering::Relaxed);
}

/// Lets a subject's next event through once its predecessor has finished.
fn release<E: Occurrence>(
    shared: &Arc<Shared<E>>,
    queue: &mut Pending<E>,
    subject: Option<E::Subject>,
) {
    if let Some(subject) = subject {
        queue.finish(&subject);
    }
    shared.meters.in_flight.fetch_sub(1, Ordering::Relaxed);
}

/// Delivers one event, retrying until it lands or the attempts run out.
///
/// Retries happen here rather than by re-queuing, so the subject's slot stays
/// held for the whole thing. That is deliberate: releasing it between attempts
/// would let the next event overtake the one being retried, which is exactly
/// the reordering the per-subject rule exists to prevent. The cost is that one
/// unreachable endpoint stalls that subject for up to
/// `maximum_attempts` × backoff.
async fn send<E: Occurrence>(
    shared: Arc<Shared<E>>,
    client: HttpClient,
    observer: Arc<dyn HookObserver<E::Kind>>,
    envelope: Item<E>,
) -> E::Subject {
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
                shared.meters.delivered.fetch_add(1, Ordering::Relaxed);
                return envelope.subject;
            }
            Verdict::Rejected => {
                report(&shared, &observer, &envelope, Loss::Rejected, &result);
                return envelope.subject;
            }
            Verdict::Retry if attempt == shared.config.maximum_attempts => {
                report(&shared, &observer, &envelope, Loss::Exhausted, &result);
                return envelope.subject;
            }
            Verdict::Retry => {
                shared.meters.retried.fetch_add(1, Ordering::Relaxed);
                // An endpoint asking to be left alone knows better than the backoff
                // curve does, so its answer wins — but only up to the cap, since it is
                // also the party currently misbehaving.
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
            // Transient by definition, plus the two the specifications reserve for
            // "ask again": a request timeout and too-early.
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_EARLY | StatusCode::TOO_MANY_REQUESTS => {
                Verdict::Retry
            }
            status if status.is_server_error() => Verdict::Retry,
            // Any other 4xx is the consumer saying the request itself is wrong.
            // Retrying would move a poison event to the front of this subject's queue
            // for the whole backoff, delaying everything behind it.
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

fn report<E: Occurrence>(
    shared: &Arc<Shared<E>>,
    observer: &Arc<dyn HookObserver<E::Kind>>,
    envelope: &Item<E>,
    loss: Loss,
    result: &Result<Response, OutboundError>,
) {
    shared.meters.record_loss(loss);
    observer.event_dropped(
        &shared.config.name,
        &envelope.id,
        envelope.kind,
        loss,
        &match result {
            Ok(response) => format!("HTTP {}", response.status),
            Err(error) => error.to_string(),
        },
    );
}

/// Finishes what is in flight, then what is queued, until the deadline.
async fn drain<E: Occurrence>(
    shared: Arc<Shared<E>>,
    mut ingress: mpsc::Receiver<Item<E>>,
    mut queue: Pending<E>,
    client: HttpClient,
    timeout: Duration,
    observer: Arc<dyn HookObserver<E::Kind>>,
    mut sending: JoinSet<E::Subject>,
) {
    let deadline = tokio::time::Instant::now() + timeout;

    let drained = tokio::time::timeout_at(deadline, async {
        loop {
            drain_ingress(&shared, &mut ingress, &mut queue);
            start_available(&shared, &client, &observer, &mut queue, &mut sending);
            let Some(finished) = sending.join_next().await else {
                break;
            };
            release(&shared, &mut queue, finished.ok());
        }
    })
    .await;

    if drained.is_err() {
        sending.abort_all();
        let mut unknown = 0;
        // Reap after cancellation rather than counting `JoinSet::len`: a task may
        // have completed at the deadline without yet being joined, in which case
        // its delivered or loss counter is already definitive.
        while let Some(finished) = sending.join_next().await {
            if finished.is_err() {
                unknown += 1;
            }
        }
        shared
            .meters
            .outcome_unknown_shutdown
            .fetch_add(unknown as u64, Ordering::Relaxed);
        if unknown > 0 {
            observer.delivery_outcomes_unknown(&shared.config.name, unknown);
        }
    }
    // Whatever is still waiting in either bounded stage is gone: it lives in
    // memory only, and the process is on its way out.
    let mut lost = queue.clear();
    while ingress.try_recv().is_ok() {
        shared.meters.ingress_depth.fetch_sub(1, Ordering::Relaxed);
        lost += 1;
    }
    shared.meters.queue_depth.store(0, Ordering::Relaxed);
    shared.meters.in_flight.store(0, Ordering::Relaxed);
    for _ in 0..lost {
        shared.meters.record_loss(Loss::Shutdown);
    }
    if lost > 0 {
        observer.events_abandoned(&shared.config.name, lost);
    }
}
