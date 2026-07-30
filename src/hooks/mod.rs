//! Telling other systems what happened, without ever slowing this one down.
//!
//! # What is promised
//!
//! Best-effort, at-least-once while the process lives. Every event carries a
//! stable id that retries reuse, so a consumer deduplicates on `source` + `id`.
//! Events for one stream arrive in the order they occurred, though a prefix may
//! be missing; nothing is promised across streams.
//!
//! Events are lost by queue overflow, permanent rejection, exhausted retries,
//! or the drain deadline at shutdown, each counted separately because they mean
//! different things. Nothing survives a crash.
//!
//! **Hooks are not a substitute for reading state.** Anything correctness-
//! critical has to reconcile. If that ever stops being acceptable the answer is
//! a durable outbox, not more retry settings.
//!
//! # Why it cannot stall a publication
//!
//! [`Hooks::deliver`] only locks a queue, pushes, and returns — no awaiting, no
//! network. It is called from
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
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::{
    domain::{SessionId, StreamId},
    observe::{
        EventObserver, Events, NodeEvent, SessionEvent, StreamEvent,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
pub enum Loss {
    /// The queue was full, so the oldest waiting event made room.
    #[display("overflow")]
    Overflow,
    /// The endpoint refused it in a way retrying cannot fix.
    #[display("rejected")]
    Rejected,
    /// Every attempt failed.
    #[display("exhausted")]
    Exhausted,
    /// Still queued when the drain deadline passed.
    #[display("shutdown")]
    Shutdown,
}

/// Delivery counters for one hook.
#[derive(Debug, Default)]
pub struct HookMeters {
    delivered: AtomicU64,
    retried: AtomicU64,
    overflow: AtomicU64,
    rejected: AtomicU64,
    exhausted: AtomicU64,
    shutdown: AtomicU64,
    /// Events for a kind this hook did not subscribe to are not losses.
    filtered: AtomicU64,
}

impl Loss {
    /// Stable enough to appear in a log or a metric label.
    pub fn as_str(self) -> &'static str {
        match self {
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
            overflow: self.overflow.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            exhausted: self.exhausted.load(Ordering::Relaxed),
            shutdown: self.shutdown.load(Ordering::Relaxed),
            filtered: self.filtered.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HookSnapshot {
    pub delivered: u64,
    pub retried: u64,
    pub overflow: u64,
    pub rejected: u64,
    pub exhausted: u64,
    pub shutdown: u64,
    pub filtered: u64,
}

/// One hook's queue, settings, and counters, shared with its dispatcher.
#[derive(Debug)]
struct Shared {
    config: HookConfig,
    queue: Mutex<Queue>,
    /// Wakes the dispatcher when work arrives.
    arrived: Notify,
    meters: HookMeters,
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
    /// Never blocks and never fails: a hook that cannot keep up drops its own
    /// oldest event and counts it. Rendering happens once even with several
    /// hooks, because the envelope is immutable and shared.
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
            if hook.queue.lock().push(envelope.clone()) {
                hook.meters.record_loss(Loss::Overflow);
            }
            hook.arrived.notify_one();
        }
    }

    /// Counters for each configured hook, in configuration order.
    pub fn snapshots(&self) -> Vec<(Arc<str>, HookSnapshot)> {
        self.hooks
            .iter()
            .map(|hook| (Arc::clone(&hook.config.name), hook.meters.snapshot()))
            .collect()
    }
}

/// Everything needed to run the configured hooks.
pub struct Dispatchers {
    shared: Vec<Arc<Shared>>,
    client: HttpClient,
    drain_timeout: Duration,
    events: Events,
}

/// Builds the enqueue side and the dispatchers that drain it.
///
/// Split so the caller owns where the dispatchers run: they belong in the
/// node's task set, alongside the listeners they outlive.
pub fn build(config: HooksConfig, client: HttpClient, events: Events) -> (Hooks, Dispatchers) {
    let shared: Vec<Arc<Shared>> = config
        .hooks
        .into_iter()
        .map(|hook| {
            Arc::new(Shared {
                queue: Mutex::new(Queue::new(hook.queue_capacity)),
                config: hook,
                arrived: Notify::new(),
                meters: HookMeters::default(),
            })
        })
        .collect();

    (
        Hooks {
            renderer: Renderer::new(config.source, config.schema_version),
            hooks: Arc::new(shared.clone()),
            events: events.clone(),
        },
        Dispatchers {
            shared,
            client,
            drain_timeout: config.drain_timeout,
            events,
        },
    )
}
