use std::sync::Arc;

use thiserror::Error;

use crate::{
    admission::{AdmissionError, PublishGrant, PublishRequest},
    domain::BoxFuture,
    observe::SourceMeters,
};

use super::PacketSource;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishRejection {
    Unauthorized,
    Forbidden,
    AlreadyPublished,
    ServiceUnavailable,
}

impl From<&AdmissionError> for PublishRejection {
    fn from(error: &AdmissionError) -> Self {
        match error {
            AdmissionError::InvalidCredential => Self::Unauthorized,
            AdmissionError::Forbidden => Self::Forbidden,
            AdmissionError::AlreadyPublished => Self::AlreadyPublished,
            AdmissionError::Service(_) => Self::ServiceUnavailable,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TransportError {
    #[error("invalid publication request: {0}")]
    InvalidPublishRequest(String),
    #[error("transport handshake failed: {0}")]
    Handshake(String),
    #[error("transport failed while accepting publication: {0}")]
    Accept(String),
    #[error("transport failed while rejecting publication: {0}")]
    Reject(String),
}

pub struct AcceptedPublish {
    pub source: Box<dyn PacketSource>,
    pub grant: PublishGrant,
}

/// A publication that has completed its protocol handshake but not admission.
///
/// `self: Box<Self>` keeps the trait object-safe while still consuming the
/// pending publication, so accepting and rejecting remain mutually exclusive by
/// construction.
pub trait PendingPublish: Send {
    fn publish_request(&self) -> Result<PublishRequest, TransportError>;

    /// Accepts the publication and assembles the demultiplexed source.
    ///
    /// The meters are created by the session and handed *down*, so the busiest
    /// producer in the system reports through the same handle as every other
    /// stage instead of owning a counter the session has to adopt.
    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>>;

    fn reject(
        self: Box<Self>,
        rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>>;
}
