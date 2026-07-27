//! What the store refuses, and why.
//!
//! Every variant names the rendition it concerns, because a publisher with
//! several outputs needs to know which one broke its contract. Validation runs
//! before any state is touched, so a rejected write leaves the rendition
//! exactly as the previous accepted write left it.

use thiserror::Error;

use crate::{
    domain::RenditionId,
    mux::{PackagingRenditionId, PackagingSegmentId},
};

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the delivery store is already holding its maximum of {maximum} streams")]
pub struct StoreFull {
    pub maximum: usize,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoreWriteError {
    #[error("media references unknown {rendition_id}")]
    UnknownPackagingRendition { rendition_id: PackagingRenditionId },
    #[error("{rendition_id} is not active in the current publication")]
    RenditionInactive { rendition_id: RenditionId },
    #[error("media arrived for {rendition_id} before an initialization segment")]
    InitializationMissing { rendition_id: RenditionId },
    #[error("an initialization segment arrived while {rendition_id} had an open segment")]
    InitializationDuringOpenSegment { rendition_id: RenditionId },
    #[error(
        "{rendition_id} emitted packaging segment {found:?} after {previous:?}; \
         packaging segment identifiers must increase"
    )]
    NonMonotonicSegmentId {
        rendition_id: RenditionId,
        previous: PackagingSegmentId,
        found: PackagingSegmentId,
    },
    #[error("{rendition_id} emitted packaging segment {found:?} while {open:?} was still open")]
    DifferentSegmentAlreadyOpen {
        rendition_id: RenditionId,
        open: PackagingSegmentId,
        found: PackagingSegmentId,
    },
    #[error("{rendition_id} emitted chunk index {found} but the open segment expected {expected}")]
    UnexpectedChunkIndex {
        rendition_id: RenditionId,
        expected: u32,
        found: u32,
    },
    #[error("{rendition_id} completed a segment when none was open")]
    NoOpenSegment { rendition_id: RenditionId },
    #[error("{rendition_id} completed packaging segment {found:?}, but {open:?} was open")]
    WrongSegmentCompleted {
        rendition_id: RenditionId,
        open: PackagingSegmentId,
        found: PackagingSegmentId,
    },
    #[error("{rendition_id}'s completed segment timing does not match its chunks")]
    SegmentTimingMismatch { rendition_id: RenditionId },
    #[error("{rendition_id} emitted a direct segment while another segment was open")]
    DirectSegmentDuringOpenSegment { rendition_id: RenditionId },
    #[error("{rendition_id} emitted a chunk for a segment-only rendition")]
    ChunksDisabled { rendition_id: RenditionId },
    #[error("{rendition_id} emitted a direct segment for a chunked rendition")]
    DirectSegmentsDisabled { rendition_id: RenditionId },
    #[error(
        "retaining another {additional} bytes would exceed the stream payload budget of {maximum}"
    )]
    PayloadCapacityExceeded { maximum: usize, additional: usize },
    #[error("retaining another part would exceed the stream part limit of {maximum}")]
    PartCapacityExceeded { maximum: usize },
    #[error("retaining another segment would exceed the stream segment limit of {maximum}")]
    SegmentCapacityExceeded { maximum: usize },
}
