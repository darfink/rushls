//! Rendering a lifecycle event into the bytes that go on the wire.
//!
//! CloudEvents, because it settles `id`, `source`, `type`, and `time` in a way
//! consumers already have libraries for, without committing anyone to a broker.
//!
//! The whole envelope is built at the moment the fact occurs and never touched
//! again. That is what makes retries safe to deduplicate: the specification
//! anticipates redelivery under the same `id`, so a consumer keying on
//! `source` + `id` sees one occurrence however many times it arrives.

use bytes::Bytes;
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    domain::StreamId,
    observe::lifecycle::{self, Event},
};

/// Structured-mode CloudEvents, as the specification names it.
pub const CONTENT_TYPE: &str = "application/cloudevents+json";

const SPEC_VERSION: &str = "1.0";
const JSON: &str = "application/json";

/// Prefix for every `type` this node emits.
///
/// Deliberately not reverse-DNS. The convention is a SHOULD in the
/// specification, and it exists so producers sharing a bus can be told apart —
/// a job `source` already does here, since that is what an operator sets per
/// deployment and what consumers deduplicate on. Claiming a domain this
/// project does not own to repeat that would be a statement about semantics
/// ownership that is not true.
///
/// Fixed rather than configurable: `type` is what a consumer routes on, so an
/// operator changing it would break the very contract the name exists to keep.
/// A fork changes this line.
const TYPE_PREFIX: &str = "rushls";

/// One rendered event, immutable from here on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Envelope {
    /// Stable across every retry of this occurrence.
    pub id: String,
    /// The stream, which is also what delivery orders by.
    pub subject: StreamId,
    pub kind: lifecycle::Kind,
    pub body: Bytes,
}

#[derive(Serialize)]
struct CloudEvent<'a> {
    specversion: &'static str,
    id: &'a str,
    source: &'a str,
    #[serde(rename = "type")]
    kind: String,
    time: String,
    subject: &'a str,
    datacontenttype: &'static str,
    data: serde_json::Value,
}

/// Builds envelopes for one node.
#[derive(Clone, Debug)]
pub struct Renderer {
    source: String,
    schema_version: u32,
}

impl Renderer {
    pub fn new(source: impl Into<String>, schema_version: u32) -> Self {
        Self {
            source: source.into(),
            schema_version,
        }
    }

    /// Renders now, at the moment the occurrence happened.
    ///
    /// `id` and `time` are stamped together and here rather than at send time:
    /// a queued event describes when it happened, not when delivery got round
    /// to it, and a retry must not look like a second occurrence.
    pub fn render(&self, event: &Event) -> Result<Envelope, RenderError> {
        let id = Uuid::now_v7().to_string();
        let kind = event.kind();
        let subject = event.stream().clone();
        let body = {
            let cloud_event = CloudEvent {
                specversion: SPEC_VERSION,
                id: &id,
                source: &self.source,
                kind: format!("{TYPE_PREFIX}.{kind}.v{}", self.schema_version),
                time: OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .map_err(|error| RenderError(error.to_string()))?,
                subject: subject.0.as_str(),
                datacontenttype: JSON,
                data: data(event),
            };
            Bytes::from(
                serde_json::to_vec(&cloud_event).map_err(|error| RenderError(error.to_string()))?,
            )
        };

        Ok(Envelope {
            id,
            subject,
            kind,
            body,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("a lifecycle event could not be rendered: {0}")]
pub struct RenderError(String);

fn data(event: &Event) -> serde_json::Value {
    match event {
        Event::SessionStarted(started) => serde_json::json!({
            "stream_id": started.stream.0.as_str(),
            "session_id": session_id(started.session),
            "principal": started.principal,
        }),
        // Stream-lifetime events carry no session: they are about the stream,
        // which outlives whichever publisher happened to make it playable.
        Event::StreamAvailable(available) => serde_json::json!({
            "stream_id": available.stream.0.as_str(),
        }),
        Event::StreamUnavailable(unavailable) => serde_json::json!({
            "stream_id": unavailable.stream.0.as_str(),
        }),
        Event::SessionEnded(ended) => serde_json::json!({
            "stream_id": ended.stream.0.as_str(),
            "session_id": session_id(ended.session),
            "principal": ended.principal,
            "outcome": ended.outcome.to_string(),
            "duration_ms": u64::try_from(ended.duration.as_millis()).unwrap_or(u64::MAX),
            "was_available": ended.was_available,
            "diagnostic": ended.diagnostic,
        }),
    }
}

/// Session ids are rendered as strings.
///
/// They are 64-bit, and JSON numbers land in a double in every browser and in
/// most JavaScript-based consumers, which silently loses precision past 2^53.
fn session_id(session: crate::domain::SessionId) -> String {
    session.0.to_string()
}
