//! This node's lifecycle events, delivered to the endpoints an operator named.
//!
//! The delivery machinery — CloudEvents rendering, per-stream ordering,
//! drop-oldest overflow, retry, and the bounded drain at shutdown — lives in
//! `rushls-hooks`. What stays here is everything that
//! is about *this* node: which facts it promises, how a stream is spelled as a
//! subject, what each event carries, and how delivery failures are reported
//! through the observer the rest of the process already uses.
//!
//! See the shared crate for what is and is not promised about delivery. The
//! short version is best-effort and at-least-once while the process lives,
//! ordered per stream, and never a substitute for reading state.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use rushls_hooks::{HookObserver as HookReporter, Loss, Occurrence};

use crate::{
    domain::{SessionId, StreamId},
    observe::{
        EventObserver, Events, NodeEvent, SessionEvent, StreamEvent,
        counters::series,
        lifecycle::{self, Event, Projector},
    },
};

pub use rushls_hooks::{
    CONTENT_TYPE, Envelope, HookSnapshot, Loss as HookLoss, RenderError, Renderer,
};

/// This node's hooks, with its own event vocabulary bound in.
pub type Hooks = rushls_hooks::Hooks<Event>;
pub type Dispatchers = rushls_hooks::Dispatchers<Event>;
pub type HookConfig = rushls_hooks::HookConfig<lifecycle::Kind>;
pub type HooksConfig = rushls_hooks::HooksConfig<lifecycle::Kind>;

impl rushls_hooks::Subject for StreamId {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Occurrence for Event {
    type Subject = StreamId;
    type Kind = lifecycle::Kind;

    /// A fork changes this line. See [`Occurrence::TYPE_PREFIX`] for why it is
    /// neither reverse-DNS nor configurable.
    const TYPE_PREFIX: &'static str = "rushls";

    fn kind(&self) -> Self::Kind {
        Event::kind(self)
    }

    fn subject(&self) -> Self::Subject {
        self.stream().clone()
    }

    fn data(&self) -> serde_json::Value {
        match self {
            Event::SegmentReady(event) => {
                let segment = &event.segment;
                serde_json::json!({
                    "stream_id": event.stream.0.as_str(),
                    "rendition_id": segment.rendition_id,
                    "segment_id": segment.segment_id.to_string(),
                    "media_sequence": segment.media_sequence.to_string(),
                    "publication": segment.publication.to_string(),
                    "path": segment.path,
                    "initialization_path": segment.initialization_path,
                    "media_start": segment.media_start.to_string(),
                    "duration": segment.duration.to_string(),
                    "timebase": {"numerator": segment.timebase.num().get(), "denominator": segment.timebase.den().get()},
                    "bytes": segment.bytes.to_string(),
                    "independent": segment.independent,
                    "discontinuity": segment.discontinuity,
                })
            }
            Event::SessionDegraded(event) | Event::SessionRecovered(event) => serde_json::json!({
                "stream_id": event.stream.0.as_str(), "session_id": session_id(event.session), "compensation": recovery_data(&event.status),
            }),
            Event::SessionStarted(started) => serde_json::json!({
                "stream_id": started.stream.0.as_str(),
                "session_id": session_id(started.session),
                "principal": started.principal,
                "protocol": started.publisher.protocol,
                "resource": started.publisher.resource,
                "client": started.publisher.client,
            }),
            // Stream-lifetime events carry no session: they are about the
            // stream, which outlives whichever publisher made it playable.
            Event::StreamAvailable(available) => serde_json::json!({
                "stream_id": available.stream.0.as_str(),
                // Rooted like `segment.ready` paths: join it to the origin.
                "playlist_path": crate::delivery::hls::uri::multivariant_path(&available.stream),
            }),
            Event::StreamUnavailable(unavailable) => serde_json::json!({
                "stream_id": unavailable.stream.0.as_str(),
            }),
            Event::SessionEnded(ended) => {
                let mut body = serde_json::json!({
                    "stream_id": ended.stream.0.as_str(),
                    "session_id": session_id(ended.session),
                    "principal": ended.principal,
                    "protocol": ended.publisher.protocol,
                    "resource": ended.publisher.resource,
                    "client": ended.publisher.client,
                    "outcome": ended.outcome.to_string(),
                    "duration_ms": u64::try_from(ended.duration.as_millis()).unwrap_or(u64::MAX),
                    "was_available": ended.was_available,
                    "diagnostic": ended.diagnostic,
                });
                if !ended.compensation.is_empty() {
                    body["compensation"] = ended
                        .compensation
                        .iter()
                        .map(recovery_data)
                        .collect::<Vec<_>>()
                        .into();
                }
                if let Some(issue) = &ended.timestamp_issue {
                    body["timestamp_issue"] = serde_json::json!({
                        "code": issue.code.to_string(), "track_id": issue.track.0,
                        "media_kind": format!("{:?}", issue.media_kind).to_lowercase(),
                        "codec": format!("{:?}", issue.codec).to_lowercase(),
                        "field": issue.field.to_string(),
                        "reference": issue.reference.to_string(), "actual": issue.actual.to_string(),
                        "timebase": {"numerator": issue.timebase.num().get(), "denominator": issue.timebase.den().get()},
                        "tolerance_ticks": issue.tolerance_ticks.map(|value| value.to_string()),
                        "maximum_ns": issue.maximum.map(|value| value.as_nanos().to_string()),
                        "missing_ticks": issue.missing_ticks.map(|value| value.to_string()),
                    });
                    if let Some(cadence) = issue.cadence {
                        body["timestamp_issue"]["cadence"] = cadence_data(cadence);
                    }
                    if let Some(reason) = issue.recovery_rejection {
                        body["timestamp_issue"]["recovery_rejection"] = reason.to_string().into();
                    }
                }
                body
            }
        }
    }
}

fn recovery_data(status: &crate::domain::CompensationStatus) -> serde_json::Value {
    serde_json::json!({
        "media_kind": format!("{:?}", status.media_kind).to_lowercase(), "cadence": status.cadence.map(cadence_data),
        "track_id": status.track.0, "codec": format!("{:?}", status.codec).to_lowercase(), "method": status.method.to_string(),
        "timebase": {"numerator": status.timebase.num().get(), "denominator": status.timebase.den().get()},
        "missing_ticks": status.missing_ticks.to_string(), "replacement_ticks": status.replacement_ticks.to_string(),
        "episode_holes": status.episode_holes.to_string(), "episode_ticks": status.episode_ticks.to_string(),
        "total_holes": status.total_holes.to_string(), "total_ticks": status.total_ticks.to_string(), "degraded": status.degraded,
    })
}

/// Session ids are rendered as strings.
///
/// They are 64-bit, and JSON numbers land in a double in every browser and in
/// most JavaScript-based consumers, which silently loses precision past 2^53.
fn session_id(session: SessionId) -> String {
    session.0.to_string()
}

/// Reports the delivery subsystem's own failures the way this node reports
/// everything else.
///
/// Deliberately the same sink the rest of the process uses, so an embedder that
/// redirects output redirects all of it. Emitting here cannot loop back: only
/// lifecycle events reach [`Hooks::deliver`], and these are node events.
/// Anything that later projects `NodeEvent` into the public vocabulary must
/// keep it that way, or a dead endpoint would generate failure events addressed
/// to the dead endpoint.
#[derive(Clone, Debug)]
pub struct HookEvents(Events);

impl HookEvents {
    pub fn new(events: Events) -> Self {
        Self(events)
    }
}

impl HookReporter<lifecycle::Kind> for HookEvents {
    fn event_dropped(
        &self,
        hook: &str,
        event: &str,
        kind: lifecycle::Kind,
        loss: Loss,
        detail: &str,
    ) {
        self.0.emit(NodeEvent::HookEventDropped {
            hook: hook.into(),
            event: event.to_owned(),
            kind,
            reason: loss.as_str(),
            detail: detail.to_owned(),
        });
    }

    fn events_abandoned(&self, hook: &str, dropped: usize) {
        self.0.emit(NodeEvent::HookEventsAbandoned {
            hook: hook.into(),
            dropped,
        });
    }

    fn delivery_outcomes_unknown(&self, hook: &str, count: usize) {
        self.0.emit(NodeEvent::HookDeliveryOutcomesUnknown {
            hook: hook.into(),
            count,
        });
    }

    fn event_unrenderable(&self, reason: &str) {
        self.0.emit(NodeEvent::HookEventUnrenderable {
            reason: reason.to_owned(),
        });
    }
}

// The losses are kept apart rather than summed into one "failed" counter: an
// endpoint refusing an event, a queue overflowing, and a shutdown cutting a
// drain short call for three different responses from whoever is looking.
//
// Two of these are not measured at all: the capacities are what an operator
// configured, and are filled in by `Hooks::snapshots` from the config rather
// than read from a counter.
//
// Declared here rather than in `rushls-hooks` because the names are this node's
// metric contract: the shared crate exports the numbers, and each application
// decides what to call them.
series! {
    /// Every series a hook exports, in declaration order.
    pub HOOK_SERIES: HookSnapshot {
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
        if let Some(projected) = self.projector.project_stream(stream.clone(), event.clone()) {
            self.hooks.deliver(&projected);
        }
        self.inner.observe_stream(stream, event);
    }

    fn observe_node(&self, event: NodeEvent) {
        self.inner.observe_node(event);
    }
}

/// Builds the enqueue side and the dispatchers that drain it.
///
/// Thin over [`rushls_hooks::build`], binding this node's event vocabulary and
/// routing the subsystem's own failures into `events`.
pub fn build(
    config: HooksConfig,
    client: crate::outbound::HttpClient,
    events: Events,
) -> Result<(Hooks, Dispatchers), rushls_hooks::ConfigError> {
    rushls_hooks::build(config, client, Arc::new(HookEvents::new(events)))
}

fn cadence_data(cadence: crate::domain::VideoCadence) -> serde_json::Value {
    use crate::domain::VideoCadence as C;
    let mut value = serde_json::json!({"source": cadence.source().map(|s| s.to_string())});
    value["kind"] = match cadence {
        C::Unknown => "unknown",
        C::Nominal(_) => "nominal",
        C::Fixed { .. } => "fixed",
        C::Unverifiable { .. } => "unverifiable",
        C::Conflicting { .. } => "conflicting",
    }
    .into();
    if let Some(rate) = cadence.rate() {
        value["interval"] = serde_json::json!({"numerator": rate.denominator().get(), "denominator": rate.numerator().get()});
    }
    if let C::Fixed { scope, .. } = cadence {
        value["scope"] = scope.to_string().into();
    }
    if let C::Unverifiable { reason, .. } = cadence {
        value["reason"] = reason.to_string().into();
    }
    value
}
