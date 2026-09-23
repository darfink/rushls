//! Telling other systems what happened, without ever slowing this one down.
//!
//! # What is promised
//!
//! Best-effort, at-least-once while the process lives. Every event carries a
//! stable id that retries reuse, so a consumer deduplicates on `source` + `id`.
//! Events for one subject arrive in the order they occurred, though a prefix
//! may be missing; nothing is promised across subjects.
//!
//! Events are lost by exceptional ingress saturation, queue overflow, permanent
//! rejection, exhausted retries, or the drain deadline at shutdown, each counted
//! separately because they mean different things. Nothing survives a crash.
//!
//! **Hooks are not a substitute for reading state.** Anything correctness-
//! critical has to reconcile. If that ever stops being acceptable the answer is
//! a durable outbox, not more retry settings.
//!
//! # Why it cannot stall the caller
//!
//! [`Hooks::deliver`] renders once, performs only bounded `try_send` operations,
//! and returns — no waiting, no queue scans, no network. Callers invoke it from
//! whatever task produced the event, often a media session's own, so anything
//! slower would pause that work for whatever a remote endpoint felt like taking.
//!
//! # What an application supplies
//!
//! This crate owns the *transport*: CloudEvents rendering, per-subject
//! ordering, drop-oldest overflow, retry with backoff, and a bounded drain at
//! shutdown. It deliberately owns none of the *vocabulary*.
//!
//! An application implements [`Occurrence`] for its own event type, naming the
//! [`Subject`] that ordering follows and the [`Kind`] that subscriptions
//! filter on. Two applications therefore share the machinery while promising
//! their consumers entirely different facts.

mod dispatch;
mod envelope;
mod queue;
mod signing;
pub use signing::{InvalidSigningSecret, SigningSecret};

#[cfg(test)]
mod tests;

use std::{
    collections::BTreeSet,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::mpsc;

use rushls_outbound::{BearerToken, Endpoint, HttpClient};

pub use envelope::{CONTENT_TYPE, Envelope, RenderError, Renderer};

use queue::Queue;

/// What delivery orders by: one subject's events keep their relative order.
///
/// Typically a stream or session identity. It is a hash key and a metrics-free
/// value, so the bounds are only what the queue needs, plus a borrowed string
/// for the CloudEvents `subject` attribute.
///
/// `Sync` because a hook's configuration and its in-flight envelopes are shared
/// behind an `Arc` across the dispatcher's tasks.
pub trait Subject: Clone + Eq + Hash + Send + Sync + 'static {
    /// How this subject is spelled in the envelope.
    fn as_str(&self) -> &str;
}

/// Which fact an event reports, and what a subscription list is written in.
///
/// `Display` is what appears in the CloudEvents `type`, so it is part of an
/// application's public contract with its consumers.
pub trait Kind: Copy + Eq + Ord + Hash + std::fmt::Display + Send + Sync + 'static {}

impl<T> Kind for T where T: Copy + Eq + Ord + Hash + std::fmt::Display + Send + Sync + 'static {}

/// One thing an application promises to report.
///
/// The vocabulary lives with the application that owns it. This crate never
/// interprets `data`; it only stamps it with an id, a time, and a type.
pub trait Occurrence: Send + 'static {
    type Subject: Subject;
    type Kind: Kind;

    /// Prefix for every `type` an application emits, such as `rushls`.
    ///
    /// Deliberately not reverse-DNS, and deliberately not configurable. The
    /// convention is a SHOULD in the specification and exists so producers
    /// sharing a bus can be told apart — a job `source` already does, since that
    /// is what an operator sets per deployment and what consumers deduplicate on.
    /// `type` is also what a consumer routes on, so letting an operator change it
    /// would break the very contract the name exists to keep.
    const TYPE_PREFIX: &'static str;

    fn kind(&self) -> Self::Kind;
    fn subject(&self) -> Self::Subject;
    /// The CloudEvents `data` member, rendered once per occurrence.
    fn data(&self) -> serde_json::Value;
}

/// Where this crate reports its own failures.
///
/// A trait rather than a concrete sink because two applications count and log
/// differently, and neither should have to adopt the other's observability
/// model to reuse the delivery machinery. Every method has a default, so an
/// application that wants none of it implements nothing.
///
/// Implementations are called from the dispatcher task and must not block.
///
/// Nothing reported here may be fed back into [`Hooks::deliver`]: a dead
/// endpoint would then generate failure events addressed to the dead endpoint.
///
/// Generic over the application's [`Kind`] rather than taking a rendered
/// string, so an application that already has a typed event vocabulary keeps it
/// when reporting a failure against one.
pub trait HookObserver<K: Kind>: Send + Sync + 'static {
    /// An event will not be attempted again. `detail` is an HTTP status or a
    /// transport error, and `event` is the CloudEvents id a consumer would quote.
    fn event_dropped(&self, _hook: &str, _event: &str, _kind: K, _loss: Loss, _detail: &str) {}
    /// Queued events were abandoned at the shutdown drain deadline.
    fn events_abandoned(&self, _hook: &str, _dropped: usize) {}
    /// Deliveries were in flight when the drain deadline passed, so they may have
    /// reached the endpoint before being cancelled. Unknown rather than dropped.
    fn delivery_outcomes_unknown(&self, _hook: &str, _count: usize) {}
    /// An event could not be turned into bytes at all.
    ///
    /// A fault in this process rather than in delivery, so it names no hook:
    /// nothing was addressed yet when it failed.
    fn event_unrenderable(&self, _reason: &str) {}
}

/// Discards everything, for a caller that wants no reporting.
#[derive(Clone, Copy, Debug, Default)]
pub struct IgnoreHookEvents;

impl<K: Kind> HookObserver<K> for IgnoreHookEvents {}

/// One configured destination.
#[derive(Clone, derive_more::Debug)]
pub struct HookConfig<K> {
    /// Identifies this hook in metrics and logs, so it must be unique.
    pub name: Arc<str>,
    pub endpoint: Endpoint,
    /// Only these are delivered. Explicit rather than defaulting to everything,
    /// so a consumer written today cannot be sent an event type added later.
    pub events: BTreeSet<K>,
    /// Capacity of both bounded stages. Ingress is normally empty; the
    /// dispatcher-owned stage is the durable in-memory backlog that applies
    /// drop-oldest while an endpoint is unhealthy.
    pub queue_capacity: usize,
    /// Distinct subjects that may be in flight at once. One request per subject
    /// is the ordering rule, so this is also the concurrency.
    pub maximum_in_flight: usize,
    /// Attempts per event, the first included.
    pub maximum_attempts: u32,
    #[debug(skip)]
    pub bearer: Option<BearerToken>,
    /// Optional Standard Webhooks HMAC-SHA256 signing key.
    pub signing_secret: Option<SigningSecret>,
    /// A client of this destination's own, or `None` to use the shared one.
    ///
    /// Present only when a destination asked for something a shared pool
    /// cannot give it: a client certificate, or an authority pinned in place
    /// of the platform store. Both belong to *this* endpoint, and a pooled
    /// connection reused across endpoints would present one service's identity
    /// to another — so asking for either costs a pool, and only the endpoints
    /// that ask pay for one.
    #[debug(skip)]
    pub client: Option<HttpClient>,
}

/// A destination that cannot deliver must fail before any dispatcher starts.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("hook `{hook}`: {reason}")]
pub struct ConfigError {
    pub hook: Arc<str>,
    pub reason: &'static str,
}

impl<K> HookConfig<K> {
    /// Checks transport invariants independently of the application's parser.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let reason = if self.events.is_empty() {
            Some("subscribes to no events")
        } else if self.queue_capacity == 0 {
            Some("queue capacity must be nonzero")
        } else if self.queue_capacity > tokio::sync::Semaphore::MAX_PERMITS {
            Some("queue capacity exceeds the channel limit")
        } else if self.maximum_in_flight == 0 {
            Some("maximum in-flight requests must be nonzero")
        } else if self.maximum_attempts == 0 {
            Some("maximum attempts must be nonzero")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(ConfigError {
                hook: self.name.clone(),
                reason,
            }),
            None => Ok(()),
        }
    }
}

/// Process-wide hook settings.
#[derive(Clone, Debug)]
pub struct HooksConfig<K> {
    /// CloudEvents `source`; with the event id it identifies an occurrence.
    ///
    /// Must be stable across restarts. Nodes may share one, in which case
    /// consumers see a single logical producer.
    pub source: String,
    /// Versions the whole event vocabulary as one family.
    ///
    /// Additive changes keep the version: consumers must ignore fields they
    /// do not recognize. Removing or redefining a field bumps it, and a
    /// subscription names the kind, so it receives every version of that kind.
    pub schema_version: u32,
    /// How long a shutdown waits for queued events before abandoning them.
    pub drain_timeout: Duration,
    pub hooks: Vec<HookConfig<K>>,
}

impl<K> HooksConfig<K> {
    /// `source` has no sensible default: it identifies the deployment.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            schema_version: 1,
            drain_timeout: Duration::from_secs(5),
            hooks: Vec::new(),
        }
    }
}

/// Why an event never reached its endpoint.
///
/// Spelled once, in [`Loss::as_str`], so an application reporting these does
/// not restate the five names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
#[display("{}", self.as_str())]
pub enum Loss {
    /// The dispatcher ingress channel was unavailable, so the new event could not
    /// reach the queue that implements the normal drop-oldest policy.
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

/// One hook's operational numbers at an instant.
///
/// The losses are kept apart rather than summed into one "failed" counter: an
/// endpoint refusing an event, a queue overflowing, and a shutdown cutting a
/// drain short call for three different responses from whoever is looking.
///
/// How these are exported is the application's business — this crate has no
/// opinion about Prometheus — so the field names are the whole contract.
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
    /// What an operator configured, rather than something measured.
    pub ingress_capacity: usize,
    pub queue_depth: usize,
    /// What an operator configured, rather than something measured.
    pub queue_capacity: usize,
    pub in_flight: usize,
}

/// One hook's queue, settings, and counters, shared with its dispatcher.
#[derive(Debug)]
struct Shared<E: Occurrence> {
    config: HookConfig<E::Kind>,
    /// The producer only performs a bounded `try_send`; ordering and eviction are
    /// owned exclusively by the dispatcher on the receiving side.
    ingress: mpsc::Sender<Envelope<E::Subject, E::Kind>>,
    meters: HookMeters,
}

struct Dispatcher<E: Occurrence> {
    shared: Arc<Shared<E>>,
    ingress: mpsc::Receiver<Envelope<E::Subject, E::Kind>>,
}

/// The enqueue side, held by whatever observes application events.
#[derive(derive_more::Debug)]
pub struct Hooks<E: Occurrence> {
    renderer: Renderer<E>,
    shared: Arc<Vec<Arc<Shared<E>>>>,
    #[debug(skip)]
    observer: Arc<dyn HookObserver<E::Kind>>,
}

// Derived `Clone` would demand `E: Clone`, which the event type has no reason
// to be: everything here is behind an `Arc` or is itself cheap to clone.
impl<E: Occurrence> Clone for Hooks<E> {
    fn clone(&self) -> Self {
        Self {
            renderer: self.renderer.clone(),
            shared: Arc::clone(&self.shared),
            observer: Arc::clone(&self.observer),
        }
    }
}

impl<E: Occurrence> Hooks<E> {
    /// Renders an event once and offers it to every subscribed hook.
    ///
    /// Never blocks and never fails: rendering happens once even with several
    /// hooks, then each subscribed dispatcher receives the immutable envelope
    /// through a bounded `try_send`. Its ordering queue drops the oldest event
    /// when the endpoint cannot keep up.
    pub fn deliver(&self, event: &E) {
        if self.shared.is_empty() {
            return;
        }
        let kind = event.kind();
        // Rendered only if somebody wants it, so a node with a narrow subscription
        // pays nothing for the events nobody asked for.
        if !self
            .shared
            .iter()
            .any(|hook| hook.config.events.contains(&kind))
        {
            for hook in self.shared.iter() {
                hook.meters.filtered.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        let envelope = match self.renderer.render(event) {
            Ok(envelope) => envelope,
            // Rendering fails only if the clock or the serializer does, which is a
            // process-level fault rather than a delivery one.
            Err(error) => {
                self.observer.event_unrenderable(&error.to_string());
                return;
            }
        };

        for hook in self.shared.iter() {
            if !hook.config.events.contains(&kind) {
                hook.meters.filtered.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Incremented before publishing so a receiver cannot observe the item
            // before the gauge does. A failed send rolls it back.
            hook.meters.ingress_depth.fetch_add(1, Ordering::Relaxed);
            if hook.ingress.try_send(envelope.clone()).is_err() {
                hook.meters.ingress_depth.fetch_sub(1, Ordering::Relaxed);
                hook.meters.record_loss(Loss::Ingress);
            }
        }
    }

    /// Operational snapshot for each configured hook, in configuration order.
    pub fn snapshots(&self) -> Vec<(Arc<str>, HookSnapshot)> {
        self.shared
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
pub struct Dispatchers<E: Occurrence> {
    hooks: Vec<Dispatcher<E>>,
    client: HttpClient,
    drain_timeout: Duration,
    observer: Arc<dyn HookObserver<E::Kind>>,
}

/// Builds the enqueue side and the dispatchers that drain it.
///
/// Split so the caller owns where the dispatchers run: they belong in the
/// application's task set, alongside the listeners they outlive.
/// Every destination is validated before any channel is allocated.
pub fn build<E: Occurrence>(
    config: HooksConfig<E::Kind>,
    client: HttpClient,
    observer: Arc<dyn HookObserver<E::Kind>>,
) -> Result<(Hooks<E>, Dispatchers<E>), ConfigError> {
    let mut names = BTreeSet::new();
    for hook in &config.hooks {
        hook.validate()?;
        if !names.insert(hook.name.clone()) {
            return Err(ConfigError {
                hook: hook.name.clone(),
                reason: "duplicate destination name",
            });
        }
    }
    let dispatchers: Vec<Dispatcher<E>> = config
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
    let shared: Vec<Arc<Shared<E>>> = dispatchers
        .iter()
        .map(|dispatcher| Arc::clone(&dispatcher.shared))
        .collect();

    Ok((
        Hooks {
            renderer: Renderer::new(config.source, config.schema_version),
            shared: Arc::new(shared),
            observer: Arc::clone(&observer),
        },
        Dispatchers {
            hooks: dispatchers,
            client,
            drain_timeout: config.drain_timeout,
            observer,
        },
    ))
}
