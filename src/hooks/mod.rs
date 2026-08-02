//! Telling other systems what happened, without ever slowing this one down.
//!
//! # What is promised
//!
//! Best-effort, at-least-once while the process lives. Every event carries a
//! stable id that retries reuse, so a consumer deduplicates on `source` + `id`.
//! Events for one stream arrive in the order they occurred, though a prefix may
//! be missing; nothing is promised across streams.
//!
//! Events are lost by exceptional ingress saturation, queue overflow, permanent
//! rejection, exhausted retries, or the drain deadline at shutdown, each
//! counted separately because they mean different things. Nothing survives a
//! crash.
//!
//! **Hooks are not a substitute for reading state.** Anything correctness-
//! critical has to reconcile. If that ever stops being acceptable the answer is
//! a durable outbox, not more retry settings.
//!
//! # Why it cannot stall a publication
//!
//! [`Hooks::deliver`] renders once, performs only bounded `try_send` operations,
//! and returns — no waiting, no queue scans, no network. It is called from
//! [`EventObserver::observe`](crate::observe::EventObserver), which runs inline
//! on the session's own task, so anything slower would pause segmentation or
//! draining for whatever a remote endpoint felt like taking.

mod dispatch;
mod envelope;
mod queue;

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::mpsc;

use crate::{
    domain::{SessionId, StreamId},
    observe::{
        EventObserver, Events, NodeEvent, SessionEvent, StreamEvent,
        counters::series,
        lifecycle::{Event, Kind, Projector},
    },
    outbound::{BearerToken, Endpoint, HttpClient},
};

pub use envelope::{CONTENT_TYPE, Envelope, RenderError, Renderer};

use queue::Queue;

/// One configured destination.
#[derive(Clone, derive_more::Debug)]
pub struct HookConfig {
    /// Identifies this hook in metrics and logs, so it must be unique.
    pub name: Arc<str>,
    pub endpoint: Endpoint,
    /// Only these are delivered. Explicit rather than defaulting to everything,
    /// so a consumer written today cannot be sent an event type added later.
    pub events: BTreeSet<Kind>,
    /// Capacity of both bounded stages. Ingress is normally empty; the
    /// dispatcher-owned stage is the durable in-memory backlog that applies
    /// drop-oldest while an endpoint is unhealthy.
    pub queue_capacity: usize,
    /// Distinct streams that may be in flight at once. One request per stream
    /// is the ordering rule, so this is also the concurrency.
    pub maximum_in_flight: usize,
    /// Attempts per event, the first included.
    pub maximum_attempts: u32,
    #[debug(skip)]
    pub bearer: Option<BearerToken>,
}

/// Process-wide hook settings.
#[derive(Clone, Debug)]
pub struct HooksConfig {
    /// CloudEvents `source`; with the event id it identifies an occurrence.
    ///
    /// Must be stable across restarts. Nodes may share one, in which case
    /// consumers see a single logical producer.
    pub source: String,
    /// Versions the whole event vocabulary as one family.
    pub schema_version: u32,
    /// How long a shutdown waits for queued events before abandoning them.
    pub drain_timeout: Duration,
    pub hooks: Vec<HookConfig>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            source: "urn:rushls:node".into(),
            schema_version: 1,
            drain_timeout: Duration::from_secs(5),
            hooks: Vec::new(),
        }
    }
}

/// Why an event never reached its endpoint.
///
/// Spelled once, in [`Loss::as_str`]. `NodeEvent` carries the reason as a
/// `&'static str` rather than as this type — `observe` sits below `hooks` and
/// cannot name it — so the borrowed form is the one that has to exist, and
/// `Display` forwards to it instead of repeating the five names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
#[display("{}", self.as_str())]
pub enum Loss {
    /// The dispatcher ingress channel was unavailable, so the new event could
    /// not reach the queue that implements the normal drop-oldest policy.
    Ingress,
    /// The queue was full, so the oldest waiting event made room.
    Overflow,
    /// The endpoint refused it in a way retrying cannot fix.
    Rejected,
    /// Every attempt failed.
    Exhausted,
    /// Still queued when the drain deadline passed.
    Shutdown,
}

/// Delivery counters and current saturation for one hook.
#[derive(Debug, Default)]
pub struct HookMeters {
    delivered: AtomicU64,
    retried: AtomicU64,
    ingress: AtomicU64,
    overflow: AtomicU64,
    rejected: AtomicU64,
    exhausted: AtomicU64,
    shutdown: AtomicU64,
    outcome_unknown_shutdown: AtomicU64,
    /// Events for a kind this hook did not subscribe to are not losses.
    filtered: AtomicU64,
    ingress_depth: AtomicUsize,
    queue_depth: AtomicUsize,
    in_flight: AtomicUsize,
}

impl Loss {
    /// Stable enough to appear in a log or a metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Overflow => "overflow",
            Self::Rejected => "rejected",
            Self::Exhausted => "exhausted",
            Self::Shutdown => "shutdown",
        }
    }
}

impl HookMeters {
    fn record_loss(&self, loss: Loss) {
        match loss {
            Loss::Ingress => &self.ingress,
            Loss::Overflow => &self.overflow,
            Loss::Rejected => &self.rejected,
            Loss::Exhausted => &self.exhausted,
            Loss::Shutdown => &self.shutdown,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HookSnapshot {
        HookSnapshot {
            delivered: self.delivered.load(Ordering::Relaxed),
            retried: self.retried.load(Ordering::Relaxed),
            ingress: self.ingress.load(Ordering::Relaxed),
            overflow: self.overflow.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            exhausted: self.exhausted.load(Ordering::Relaxed),
            shutdown: self.shutdown.load(Ordering::Relaxed),
            outcome_unknown_shutdown: self.outcome_unknown_shutdown.load(Ordering::Relaxed),
            filtered: self.filtered.load(Ordering::Relaxed),
            ingress_depth: self.ingress_depth.load(Ordering::Relaxed),
            ingress_capacity: 0,
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            queue_capacity: 0,
            in_flight: self.in_flight.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HookSnapshot {
    pub delivered: u64,
    pub retried: u64,
    pub ingress: u64,
    pub overflow: u64,
    pub rejected: u64,
    pub exhausted: u64,
    pub shutdown: u64,
    pub outcome_unknown_shutdown: u64,
    pub filtered: u64,
    pub ingress_depth: usize,
    pub ingress_capacity: usize,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub in_flight: usize,
}

// Declared apart from the storage above because two of these are not measured
// at all: the capacities are what an operator configured, and are filled in by
// `Hooks::snapshots` from the config rather than read from a counter.
//
// The losses are kept apart rather than summed into one "failed" counter: an
// endpoint refusing an event, a queue overflowing, and a shutdown cutting a
// drain short call for three different responses from whoever is looking.
series! {
    HookSnapshot {
        Counter("rushls_hook_deliveries_total",
            "Lifecycle events accepted by a hook endpoint.")
            = |snapshot: &HookSnapshot| snapshot.delivered,
        Counter("rushls_hook_retries_total",
            "Delivery attempts that failed and were retried.")
            = |snapshot: &HookSnapshot| snapshot.retried,
        Counter("rushls_hook_filtered_total",
            "Events not delivered because the hook did not subscribe to them.")
            = |snapshot: &HookSnapshot| snapshot.filtered,
        Counter("rushls_hook_dropped_ingress_total",
            "New events dropped because the nonblocking dispatcher ingress was unavailable.")
            = |snapshot: &HookSnapshot| snapshot.ingress,
        Counter("rushls_hook_dropped_overflow_total",
            "Events dropped because the hook's queue was full.")
            = |snapshot: &HookSnapshot| snapshot.overflow,
        Counter("rushls_hook_dropped_rejected_total",
            "Events refused by the endpoint in a way retrying cannot fix.")
            = |snapshot: &HookSnapshot| snapshot.rejected,
        Counter("rushls_hook_dropped_exhausted_total",
            "Events dropped after every delivery attempt failed.")
            = |snapshot: &HookSnapshot| snapshot.exhausted,
        Counter("rushls_hook_dropped_shutdown_total",
            "Queued events never attempted before the shutdown drain deadline.")
            = |snapshot: &HookSnapshot| snapshot.shutdown,
        Counter("rushls_hook_outcome_unknown_shutdown_total",
            "Unresolved deliveries whose outcome became unknown at shutdown.")
            = |snapshot: &HookSnapshot| snapshot.outcome_unknown_shutdown,
        Gauge("rushls_hook_ingress_depth",
            "Rendered lifecycle events waiting to enter a hook's ordering queue.")
            = |snapshot: &HookSnapshot| snapshot.ingress_depth,
        Gauge("rushls_hook_ingress_capacity",
            "Maximum rendered lifecycle events held by a hook's nonblocking ingress.")
            = |snapshot: &HookSnapshot| snapshot.ingress_capacity,
        Gauge("rushls_hook_queue_depth",
            "Lifecycle events waiting in a hook's per-stream ordering queue.")
            = |snapshot: &HookSnapshot| snapshot.queue_depth,
        Gauge("rushls_hook_queue_capacity",
            "Maximum lifecycle events held by a hook's per-stream ordering queue.")
            = |snapshot: &HookSnapshot| snapshot.queue_capacity,
        Gauge("rushls_hook_in_flight",
            "Per-stream hook deliveries currently active, including retry backoff.")
            = |snapshot: &HookSnapshot| snapshot.in_flight,
    }
}

/// One hook's queue, settings, and counters, shared with its dispatcher.
#[derive(Debug)]
struct Shared {
    config: HookConfig,
    /// The producer only performs a bounded `try_send`; ordering and eviction
    /// are owned exclusively by the dispatcher on the receiving side.
    ingress: mpsc::Sender<Envelope>,
    meters: HookMeters,
}

struct Dispatcher {
    shared: Arc<Shared>,
    ingress: mpsc::Receiver<Envelope>,
}

/// Feeds hooks from the session events a node already emits.
///
/// A decorator rather than a replacement: it forwards everything to the
/// observer it wraps, so a node keeps whatever reporting it had. What it adds
/// is the [`Projector`], which is the only thing that decides whether an
/// internal event means anything externally.
///
/// The wrapped observer is also where hooks report their own failures, which is
/// why `Hooks` is built with that one rather than with this. A node event
/// produced by a failing hook must not re-enter the hook that produced it.
#[derive(derive_more::Debug)]
pub struct HookObserver {
    projector: Projector,
    hooks: Hooks,
    #[debug(skip)]
    inner: Arc<dyn EventObserver>,
}

impl HookObserver {
    pub fn new(hooks: Hooks, inner: Arc<dyn EventObserver>) -> Self {
        Self {
            projector: Projector::new(),
            hooks,
            inner,
        }
    }
}

impl EventObserver for HookObserver {
    fn observe(&self, session: SessionId, event: SessionEvent) {
        // Projected before forwarding, because the projector holds per-session
        // state that `Ended` retires: running it first keeps that bookkeeping
        // independent of what the wrapped observer does with the event.
        if let Some(projected) = self.projector.project(session, &event) {
            self.hooks.deliver(&projected);
        }
        self.inner.observe(session, event);
    }

    fn observe_stream(&self, stream: StreamId, event: StreamEvent) {
        self.hooks
            .deliver(&self.projector.project_stream(stream.clone(), event));
        self.inner.observe_stream(stream, event);
    }

    fn observe_node(&self, event: NodeEvent) {
        self.inner.observe_node(event);
    }
}

/// The enqueue side, held by whatever observes session events.
#[derive(Clone, Debug)]
pub struct Hooks {
    renderer: Renderer,
    hooks: Arc<Vec<Arc<Shared>>>,
    /// Where this module reports its own failures.
    ///
    /// Deliberately the same sink the rest of the process uses, so an embedder
    /// that redirects output redirects all of it. Emitting here cannot loop
    /// back: only session events reach [`Hooks::deliver`], and these are node
    /// events. Anything that later projects `NodeEvent` into the public
    /// vocabulary must keep it that way, or a dead endpoint would generate
    /// failure events addressed to the dead endpoint.
    events: Events,
}

impl Hooks {
    /// Renders an event once and offers it to every subscribed hook.
    ///
    /// Never blocks and never fails: rendering happens once even with several
    /// hooks, then each subscribed dispatcher receives the immutable envelope
    /// through a bounded `try_send`. Its ordering queue drops the oldest event
    /// when the endpoint cannot keep up.
    pub fn deliver(&self, event: &Event) {
        if self.hooks.is_empty() {
            return;
        }
        let kind = event.kind();
        // Rendered only if somebody wants it, so a node with a narrow
        // subscription pays nothing for the events nobody asked for.
        if !self
            .hooks
            .iter()
            .any(|hook| hook.config.events.contains(&kind))
        {
            for hook in self.hooks.iter() {
                hook.meters.filtered.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        let envelope = match self.renderer.render(event) {
            Ok(envelope) => envelope,
            // Rendering fails only if the clock or the serializer does, which
            // is a process-level fault rather than a delivery one.
            Err(error) => {
                self.events.emit(NodeEvent::HookEventUnrenderable {
                    reason: error.to_string(),
                });
                return;
            }
        };

        for hook in self.hooks.iter() {
            if !hook.config.events.contains(&kind) {
                hook.meters.filtered.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Incremented before publishing so a receiver cannot observe the
            // item before the gauge does. A failed send rolls it back.
            hook.meters.ingress_depth.fetch_add(1, Ordering::Relaxed);
            if hook.ingress.try_send(envelope.clone()).is_err() {
                hook.meters.ingress_depth.fetch_sub(1, Ordering::Relaxed);
                hook.meters.record_loss(Loss::Ingress);
            }
        }
    }

    /// Operational snapshot for each configured hook, in configuration order.
    pub fn snapshots(&self) -> Vec<(Arc<str>, HookSnapshot)> {
        self.hooks
            .iter()
            .map(|hook| {
                let mut snapshot = hook.meters.snapshot();
                snapshot.ingress_capacity = hook.config.queue_capacity;
                snapshot.queue_capacity = hook.config.queue_capacity;
                (Arc::clone(&hook.config.name), snapshot)
            })
            .collect()
    }
}

/// Everything needed to run the configured hooks.
pub struct Dispatchers {
    hooks: Vec<Dispatcher>,
    client: HttpClient,
    drain_timeout: Duration,
    events: Events,
}

/// Builds the enqueue side and the dispatchers that drain it.
///
/// Split so the caller owns where the dispatchers run: they belong in the
/// node's task set, alongside the listeners they outlive.
pub fn build(config: HooksConfig, client: HttpClient, events: Events) -> (Hooks, Dispatchers) {
    let dispatchers: Vec<Dispatcher> = config
        .hooks
        .into_iter()
        .map(|hook| {
            let (ingress, receiver) = mpsc::channel(hook.queue_capacity);
            let shared = Arc::new(Shared {
                ingress,
                config: hook,
                meters: HookMeters::default(),
            });
            Dispatcher {
                shared,
                ingress: receiver,
            }
        })
        .collect();
    let shared: Vec<Arc<Shared>> = dispatchers
        .iter()
        .map(|dispatcher| Arc::clone(&dispatcher.shared))
        .collect();

    (
        Hooks {
            renderer: Renderer::new(config.source, config.schema_version),
            hooks: Arc::new(shared.clone()),
            events: events.clone(),
        },
        Dispatchers {
            hooks: dispatchers,
            client,
            drain_timeout: config.drain_timeout,
            events,
        },
    )
}
