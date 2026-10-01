//! Rendering an application's event into the bytes that go on the wire.
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

use crate::hooks::{Occurrence, Subject};

/// Structured-mode CloudEvents, as the specification names it.
pub const CONTENT_TYPE: &str = "application/cloudevents+json";

const SPEC_VERSION: &str = "1.0";
const JSON: &str = "application/json";

/// One rendered event, immutable from here on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Envelope<S, K> {
    /// Stable across every retry of this occurrence.
    pub id: String,
    /// What delivery orders by.
    pub subject: S,
    pub kind: K,
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
#[derive(Debug)]
pub struct Renderer<E> {
    source: String,
    schema_version: u32,
    event: std::marker::PhantomData<fn(&E)>,
}

// Hand-written because the derive would demand `E: Clone`, which an event type
// has no reason to be: the marker holds no value.
impl<E> Clone for Renderer<E> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            schema_version: self.schema_version,
            event: std::marker::PhantomData,
        }
    }
}

impl<E: Occurrence> Renderer<E> {
    pub fn new(source: impl Into<String>, schema_version: u32) -> Self {
        Self {
            source: source.into(),
            schema_version,
            event: std::marker::PhantomData,
        }
    }

    /// Renders now, at the moment the occurrence happened.
    ///
    /// `id` and `time` are stamped together and here rather than at send time: a
    /// queued event describes when it happened, not when delivery got round to
    /// it, and a retry must not look like a second occurrence.
    pub fn render(&self, event: &E) -> Result<Envelope<E::Subject, E::Kind>, RenderError> {
        let id = Uuid::now_v7().to_string();
        let kind = event.kind();
        let subject = event.subject();
        let body = {
            let cloud_event = CloudEvent {
                specversion: SPEC_VERSION,
                id: &id,
                source: &self.source,
                kind: format!("{}.{kind}.v{}", E::TYPE_PREFIX, self.schema_version),
                time: OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .map_err(|error| RenderError(error.to_string()))?,
                subject: subject.as_str(),
                datacontenttype: JSON,
                data: event.data(),
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
