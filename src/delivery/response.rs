//! Owned response vocabulary shared by protocol adapters and transports.
//!
//! These types describe bytes, media type, and safe reuse without naming an
//! HTTP implementation. The HTTP server later translates the reuse decision
//! into headers and the body into frames.

use std::time::Duration;

use bytes::Bytes;
use thiserror::Error;

use crate::delivery::{
    MediaBody,
    uri::{ContentType, MediaResource},
};

/// What one delivery request produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Body {
    /// Already-encoded manifest text.
    Manifest(Bytes),
    Media(MediaBody),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    pub body: Body,
    /// The gzip representation, when this media type has one.
    pub gzip: Option<Bytes>,
    pub content_type: ContentType,
    pub reuse: Reuse,
}

impl Response {
    pub fn media(
        body: MediaBody,
        gzip: Option<Bytes>,
        resource: MediaResource,
        kind: crate::domain::MediaKind,
        reuse: Reuse,
    ) -> Result<Self, DeliveryError> {
        debug_assert!(
            gzip.is_none() || resource.compressible(),
            "only a text media type has a gzip encoding"
        );
        Ok(Self {
            body: Body::Media(body),
            gzip,
            content_type: resource
                .content_type(kind)
                .ok_or(DeliveryError::UnknownResource)?,
            reuse,
        })
    }

    pub fn manifest(bytes: Bytes, gzip: Bytes, content_type: ContentType, reuse: Reuse) -> Self {
        debug_assert!(content_type.is_text(), "manifest text is compressible");
        Self {
            body: Body::Manifest(bytes),
            gzip: Some(gzip),
            content_type,
            reuse,
        }
    }
}

/// A failure, and how long an intermediary may remember it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
#[display("{error}")]
pub struct DeliveryFailure {
    pub error: DeliveryError,
    pub reuse: Reuse,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DeliveryError {
    #[error("no such stream")]
    UnknownStream,
    #[error("no such rendition")]
    UnknownRendition,
    #[error("no such resource, or it is no longer available")]
    UnknownResource,
    #[error("the request is not answerable: {0}")]
    InvalidDirective(&'static str),
    #[error("the requested media did not arrive within the deadline")]
    Unsatisfied,
    #[error("the manifest could not be projected")]
    Projection,
}

/// How long one response representation may be reused.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reuse {
    /// Zero asks for revalidation before every reuse.
    pub max_age: Duration,
    /// Whether the bytes remain unchanged under their name for this lifetime.
    pub immutable: bool,
}

impl Reuse {
    pub const fn revalidate() -> Self {
        Self {
            max_age: Duration::ZERO,
            immutable: false,
        }
    }

    pub const fn reusable(max_age: Duration) -> Self {
        Self {
            max_age,
            immutable: false,
        }
    }

    pub const fn immutable(max_age: Duration) -> Self {
        Self {
            max_age,
            immutable: true,
        }
    }
}
