//! Two deliberately different reporting mechanisms.
//!
//! [`SessionMeters`] carries per-item volume: bytes, packets, samples, parts.
//! Recording is a relaxed atomic add reached through a narrow trait view, and
//! callers on the streaming path accumulate batch-local totals and flush once
//! per batch. There is no dispatch table, no queue, and no allocation.
//!
//! [`SessionEvent`] carries rare structured facts: what was discovered, when
//! segmentation locked, why a session ended. These are boxed and matched
//! because they happen a handful of times per session. [`NodeEvent`] is the
//! same mechanism for facts that belong to the process rather than to any one
//! session — what got bound, which certificate is being served.
//!
//! Conflating the two is what makes reporting feel expensive. A counter routed
//! through an event bus pays dispatch to reach a `fetch_add`; a lifecycle fact
//! squeezed into a counter loses the structure that made it worth reporting.

pub mod counters;
mod delivery;
mod events;
mod histogram;
pub mod lifecycle;
mod meters;
pub use histogram::{DURATION_BUCKETS, DurationHistogram};

pub use counters::{MetricKind, Reading, Series};
pub use delivery::{HlsMeters, HlsSnapshot, OriginMeters, OriginSnapshot};
pub use events::{
    EventObserver, EventSink, Events, NodeEvent, Protocol, RetentionClipReason, SessionEnd,
    SessionEvent, StreamEvent,
};
pub use meters::{
    DeliveryMeters, MediaMeters, MeterSnapshot, MuxMeters, ProcessMeters, ProcessSnapshot,
    SessionMeters, SourceMeters,
};

mod operations;
pub use operations::{Operation, OperationMeters, OperationOutcome, OperationSnapshot};

pub mod http;

pub mod tracks;
