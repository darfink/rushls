use thiserror::Error;

use crate::{
    domain::{Appender, BoxFuture},
    source::{InputState, LimitError, SourceError},
};

use super::{MediaDensityError, NormalizeError, NormalizedSample};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum MediaError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Normalize(#[from] NormalizeError),
    /// The input asked for more than a session is allowed to cost.
    ///
    /// Distinct from the two above because it is not a fault in the input's
    /// content or in our handling of it: the media is well-formed, there is
    /// simply too much of it.
    #[error(transparent)]
    Limit(#[from] LimitError),
    #[error(transparent)]
    Density(#[from] MediaDensityError),
}

/// A running supply of normalized access units.
///
/// Pre-roll and the live loop drive the *same* supply through this trait, which
/// is what lets pre-roll borrow the pipeline instead of owning it. Without this
/// seam, planning has to take the source and normalizer by value and hand them
/// back afterwards, and the normalizer's concrete type leaks all the way into
/// the session signature.
pub trait SampleSource: Send {
    /// Appends the next batch of samples to `out`, preserving whether an
    /// exhausted input closed deliberately or was interrupted.
    fn next_batch<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<NormalizedSample>,
    ) -> BoxFuture<'a, Result<InputState, MediaError>>;
}
