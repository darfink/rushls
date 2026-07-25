//! Container encapsulation and segment cutting.
//!
//! This is where samples become bytes a player can fetch, cut to the cadence
//! [`SegmentationPlan`] fixed during pre-roll. It stays separate from
//! `delivery` because muxing is reusable — the same CMAF fragments serve HLS
//! and DASH — and separate from `media` because normalization is a codec and
//! timing concern with an entirely different dependency set.

use thiserror::Error;

use crate::{
    domain::{Appender, Payload, RenditionId, TickDuration, TickTimestamp},
    media::{NormalizedSample, PresentationPlan},
    segment::SegmentationPlan,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerFormat {
    Cmaf,
    MpegTs,
    WebVtt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MuxedMedia {
    /// Header a player must fetch before any media of this rendition.
    Initialization(InitializationSegment),
    /// A partial segment, publishable before the segment it belongs to closes.
    Part(MuxedPart),
    /// A closed segment covering the parts that preceded it.
    Segment(MuxedSegment),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializationSegment {
    pub rendition_id: RenditionId,
    pub format: ContainerFormat,
    pub version: u64,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxedPart {
    pub rendition_id: RenditionId,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub independent: bool,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxedSegment {
    pub rendition_id: RenditionId,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub payload: Payload,
}

impl MuxedMedia {
    pub fn rendition_id(&self) -> RenditionId {
        match self {
            Self::Initialization(media) => media.rendition_id,
            Self::Part(media) => media.rendition_id,
            Self::Segment(media) => media.rendition_id,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum MuxError {
    #[error("cannot mux the locked segmentation plan: {0}")]
    InvalidPlan(String),
    #[error("media muxing failed: {0}")]
    Mux(String),
}

/// What continuity delivery may expect after this muxer stops.
///
/// A muxer's last act is a judgement call — emit the short trailing part and
/// segment, or discard them — while delivery must independently decide whether
/// to signal end-of-stream. Only the session knows whether this is a deliberate
/// end, an unexplained interruption, or a known takeover, so it says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishReason {
    /// No publisher will continue this stream.
    ///
    /// Emit the open part and segment even if short: it is the last media
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
/// nothing (it is still accumulating into an open part) and occasionally
/// produces two objects at once (a part that closes a segment), which a return
/// value cannot express and a generic sink can only express by infecting every
/// caller with its type.
pub trait Muxer: Send {
    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<MuxedMedia>,
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
        out: &mut dyn Appender<MuxedMedia>,
    ) -> Result<(), MuxError>;
}

pub trait MuxerFactory: Send + Sync {
    fn start(
        &self,
        presentation: &PresentationPlan,
        segmentation: &SegmentationPlan,
    ) -> Result<Box<dyn Muxer>, MuxError>;
}
