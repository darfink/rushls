//! Who may publish, as what stream, under which constraints.
//!
//! [`StreamPolicy`] lives here rather than in `media` because it is an
//! authorization decision that happens to be expressed in media terms. Media
//! validation consumes the policy but never learns what a grant is, which keeps
//! the two modules from depending on each other.

use std::{
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU32},
    time::Duration,
};

use derive_more::{Debug, Display};
use thiserror::Error;

use crate::domain::{BoxFuture, Codec, FrameRate, StreamId};

mod http;
mod open;
mod static_stream;

pub use http::{HttpAuthConfig, HttpAuthenticator};
pub use open::OpenStreamAuthenticator;
pub use static_stream::{StaticPublisher, StaticStreamAuthenticator};

#[cfg(test)]
mod fixtures;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestProtocol {
    Rtmp,
    Srt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishResource {
    pub namespace: Option<String>,
    pub name: String,
}

impl PublishResource {
    /// Preserves the transport's requested resource as a canonical stream ID.
    ///
    /// The namespace may itself contain separators (for example an SRT
    /// resource such as `organization/live/camera`), so joining happens only
    /// at this already-normalized resource boundary.
    pub fn stream_id(&self) -> Option<StreamId> {
        if self.name.is_empty() {
            return None;
        }

        match &self.namespace {
            Some(namespace) if namespace.is_empty() => None,
            Some(namespace) => Some(StreamId::new(format!("{namespace}/{}", self.name))),
            None => Some(StreamId::new(self.name.clone())),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
#[debug("PresentedCredential([REDACTED])")]
pub struct PresentedCredential(Vec<u8>);

impl PresentedCredential {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInfo {
    pub remote_address: SocketAddr,
    pub encoder: Option<String>,
    pub protocol_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRequest {
    pub protocol: IngestProtocol,
    pub resource: PublishResource,
    pub credential: PresentedCredential,
    pub client: ClientInfo,
}

#[derive(Clone, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[display("{_0}")]
pub struct Principal(pub String);

/// Whether an authenticated publisher may replace an existing publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TakeoverPolicy {
    Deny,
    Allow,
}

/// How an admitted publication may advance relative to wall clock.
///
/// This is a stream authorization decision rather than a transport setting:
/// RTMP and SRT publications can both be either genuinely live or replayed
/// from a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestTimingPolicy {
    /// Reject a publisher once normalized media gets this far ahead.
    RequireRealtime { maximum_lead: Duration },
    /// Apply backpressure when normalized media gets ahead of wall clock.
    PaceToRealtime {
        /// Lead tolerated before the publisher is slept back to realtime.
        ///
        /// The same quantity [`Self::RequireRealtime`] rejects on, which is
        /// why both spell it the same way: how far ahead media may run. Only
        /// the answer to running further differs.
        ///
        /// Applies for the whole publication, not just its opening burst.
        maximum_lead: Duration,
        /// Reject a forward discontinuity this large rather than sleeping for
        /// what is probably a broken timeline.
        maximum_timestamp_jump: Duration,
    },
}

/// What a principal is allowed to publish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamPolicy {
    pub takeovers: TakeoverPolicy,
    pub ingest_timing: IngestTimingPolicy,
    pub accepted_video_codecs: Vec<Codec>,
    pub accepted_audio_codecs: Vec<Codec>,
    pub accepted_subtitle_codecs: Vec<Codec>,
    pub maximum_audio_tracks: usize,
    pub maximum_subtitle_tracks: usize,
    pub maximum_video_tracks: usize,
    pub maximum_video_width: NonZeroU32,
    pub maximum_video_height: NonZeroU32,
    pub maximum_video_frame_rate: FrameRate,
    pub maximum_audio_sample_rate: NonZeroU32,
    pub maximum_audio_channels: NonZeroU16,
}

impl StreamPolicy {
    /// The codec set this node can currently mux for LL-HLS delivery.
    ///
    /// # Subtitles
    ///
    /// HLS carries WebVTT, so every accepted cue format has to arrive as
    /// WebVTT or be turned into it. Those are not the same cost:
    ///
    /// - [`Codec::WebVtt`] is a pass-through. Cues are already in the output
    ///   form and the muxer only has to package them.
    /// - [`Codec::SubRip`] is converted cue-by-cue. FFmpeg has already removed
    ///   the SRT index and timing lines, leaving text plus packet timing.
    ///
    /// Accepting it here is therefore a statement about admission, not about
    /// packaging: a muxer that cannot perform that conversion must reject the
    /// track itself rather than assume it can be remuxed.
    pub fn permissive() -> Self {
        Self {
            takeovers: TakeoverPolicy::Allow,
            ingest_timing: IngestTimingPolicy::PaceToRealtime {
                maximum_lead: Duration::from_secs(2),
                maximum_timestamp_jump: Duration::from_secs(10),
            },
            accepted_video_codecs: vec![Codec::H264, Codec::Hevc, Codec::Av1],
            accepted_audio_codecs: vec![Codec::Aac, Codec::Opus],
            accepted_subtitle_codecs: vec![Codec::WebVtt, Codec::SubRip],
            maximum_audio_tracks: 8,
            maximum_subtitle_tracks: 8,
            maximum_video_tracks: 8,
            maximum_video_width: nz::u32!(7680),
            maximum_video_height: nz::u32!(4320),
            maximum_video_frame_rate: FrameRate::new(nz::u32!(240), nz::u32!(1)),
            maximum_audio_sample_rate: nz::u32!(192_000),
            maximum_audio_channels: nz::u16!(32),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PublishGrant {
    pub stream_id: StreamId,
    pub principal: Principal,
    pub policy: StreamPolicy,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AdmissionError {
    #[error("the presented credential is invalid")]
    InvalidCredential,
    #[error("the publisher is not permitted to publish this resource")]
    Forbidden,
    #[error("another publisher already owns this stream")]
    AlreadyPublished,
    #[error("admission service failed: {0}")]
    Service(Box<str>),
}

/// Resolves a protocol handshake into a grant.
///
/// Boxed because it is awaited exactly once per session; the allocation is
/// irrelevant next to the network round trip an implementation usually makes.
pub trait Authenticator: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        request: &'a PublishRequest,
    ) -> BoxFuture<'a, Result<PublishGrant, AdmissionError>>;
}
