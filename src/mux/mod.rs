//! Container encapsulation and segment cutting.
//!
//! This is where samples become bytes a player can fetch, cut to the cadence
//! [`SegmentationPlan`] fixed during pre-roll. It stays separate from
//! `delivery` because muxing is reusable — the same CMAF fragments serve HLS
//! and DASH — and separate from `media` because normalization is a codec and
//! timing concern with an entirely different dependency set.

use std::{num::NonZero, time::Duration};

use thiserror::Error;

use crate::{
    domain::{Appender, Payload, TickDuration, TickTimestamp, Timebase},
    media::NormalizedSample,
};

mod cmaf;
mod coordinator;
mod passthrough;
mod presentation;
mod track;
mod webvtt;

#[cfg(test)]
pub mod fixtures;

pub use passthrough::{PassThroughMuxerFactory, validate_timing};
pub use track::TrackPackager;

pub use presentation::{
    CaptionChannel, ClosedCaptionService, MuxerStartRequest, PackagedPresentation,
    PackagedPresentationError, PackagedRendition, PackagingRenditionId, PlayableCombination,
    RenditionGroup, RenditionGroupKey, RenditionKey, RenditionMedia, StartedMuxer, VideoRange,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaSegmentFormat {
    Cmaf,
    MpegTs,
    WebVtt,
}

/// Identifies a segment within one muxer publication and rendition.
///
/// Delivery assigns its own durable identifiers because this value may restart
/// when a publisher reconnects.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PackagingSegmentId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenditionConfig {
    pub timebase: Timebase,
    pub segment_target: NonZero<TickDuration>,
    /// The longest segment this muxer may ever emit.
    ///
    /// Equal to [`Self::segment_target`] when boundaries are exact. A policy
    /// that waits past the planned boundary for a random-access sample must
    /// declare the whole budget here, because delivery advertises a target
    /// duration that cannot change once a viewer has seen it: an extension
    /// discovered after the fact cannot be retracted from playlists that have
    /// already published the segment's parts.
    pub maximum_segment_duration: NonZero<TickDuration>,
    pub chunk_target: Option<NonZero<TickDuration>>,
    pub segment_format: MediaSegmentFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackagedMedia {
    /// Header a player must fetch before any media of this rendition.
    Initialization(InitializationSegment),
    /// A container chunk, publishable before the segment it belongs to closes.
    Chunk(PackagedChunk),
    /// A complete segment emitted without progressively published chunks.
    Segment(PackagedSegment),
    /// Closes the segment assembled from the chunks that preceded it.
    SegmentCompleted(PackagedSegmentCompletion),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializationSegment {
    pub rendition_id: PackagingRenditionId,
    pub version: u64,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackagedChunk {
    pub rendition_id: PackagingRenditionId,
    /// Publisher-local segment identity; delivery assigns a durable HLS ID.
    pub packaging_segment_id: PackagingSegmentId,
    pub chunk_index: u32,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub independent: bool,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackagedSegment {
    pub rendition_id: PackagingRenditionId,
    /// Publisher-local segment identity; delivery assigns a durable HLS ID.
    pub packaging_segment_id: PackagingSegmentId,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub independent: bool,
    pub payload: Payload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackagedSegmentCompletion {
    pub rendition_id: PackagingRenditionId,
    /// Identifies the open packaging segment being completed.
    pub packaging_segment_id: PackagingSegmentId,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
}

impl PackagedMedia {
    pub fn rendition_id(&self) -> PackagingRenditionId {
        match self {
            Self::Initialization(media) => media.rendition_id,
            Self::Chunk(media) => media.rendition_id,
            Self::Segment(media) => media.rendition_id,
            Self::SegmentCompleted(media) => media.rendition_id,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum MuxError {
    #[error(
        "{track}: {reason}; progress={progress}, permitted window=[{earliest}, {latest}] in {timebase:?}"
    )]
    BoundaryWindow {
        track: crate::domain::TrackId,
        reason: &'static str,
        progress: TickTimestamp,
        earliest: TickTimestamp,
        latest: TickTimestamp,
        timebase: Timebase,
    },
    #[error("{track}: {reason}; observed={observed}, maximum={maximum} ticks")]
    Boundary {
        track: crate::domain::TrackId,
        reason: &'static str,
        observed: u64,
        maximum: u64,
    },
    #[error("coordinator retained {samples} samples / {bytes} bytes beyond its budget")]
    CoordinatorLimit { samples: usize, bytes: usize },
    #[error("{track}: {error}")]
    Part {
        track: crate::domain::TrackId,
        error: crate::segment::cutter::CutError,
    },
    #[error("cannot mux the locked segmentation plan: {0}")]
    InvalidPlan(Box<str>),
    #[error("media muxing failed: {0}")]
    Mux(Box<str>),
}

/// What continuity delivery may expect after this muxer stops.
///
/// A muxer's last act is a judgement call — emit the short trailing chunk and
/// segment, or discard them — while delivery must independently decide whether
/// to signal end-of-stream. Only the session knows whether this is a deliberate
/// end, an unexplained interruption, or a known takeover, so it says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishReason {
    /// No publisher will continue this stream.
    ///
    /// Emit the open chunk and segment even if short: it is the last media
    /// viewers will ever get, and a truncated tail is worse than an undersized
    /// segment.
    Final,
    /// The publisher disappeared without deliberately closing the stream.
    ///
    /// Flush already accepted media, but leave the publication resumable: a
    /// reconnect may continue it and delivery must not signal end-of-stream.
    Interrupted,
    /// Another publisher has taken this stream over.
    ///
    /// Flush what is already complete, but do not manufacture a stunted final
    /// segment. The successor's media follows immediately and would have to be
    /// signalled discontinuous with it anyway.
    Superseded,
}

/// Encapsulates normalized samples into container objects.
///
/// Like [`MediaNormalizer`](crate::media::MediaNormalizer), output goes to a
/// reused caller buffer through an [`Appender`]. One sample commonly produces
/// nothing (it is still accumulating into an open chunk) and occasionally
/// produces two objects at once (a chunk and its segment completion), which a
/// return value cannot express and a generic sink can only express by infecting
/// every caller with its type.
pub trait Muxer: Send {
    /// Longest healthy interval between progressively publishable outputs.
    ///
    /// Chunked muxers report their regular chunk cadence; segment-only muxers
    /// report their segment cadence. Supervision uses this without learning
    /// either packaging mode.
    fn expected_publication_interval(&self) -> Duration;

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError>;

    /// Closes out the publication.
    ///
    /// Synchronous and prompt for the same reason as
    /// [`MediaNormalizer::finish`](crate::media::MediaNormalizer::finish): it
    /// runs on the drain path of a session that may already have been
    /// cancelled, where waiting is not an option. Repeated calls must be
    /// harmless and produce nothing further.
    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError>;
}

pub trait MuxerFactory: Send + Sync {
    /// Constructs both the byte producer and its authoritative output topology.
    ///
    /// A future transcoding muxer can therefore advertise its output ladder
    /// without pretending those properties came from input discovery.
    fn start(&self, request: MuxerStartRequest<'_>) -> Result<StartedMuxer, MuxError>;
}
