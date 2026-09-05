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

pub use http::{HttpAuthConfig, HttpAuthenticator};
pub use open::OpenStreamAuthenticator;

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

/// How fast media may be offered, as a multiple of wall clock.
///
/// An exact rational rather than a float: `1x` has to mean exactly realtime,
/// and comparing accumulated media time against wall time through a rounded
/// multiplier drifts over a long publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pace {
    numerator: NonZeroU32,
    denominator: NonZeroU32,
}

impl Pace {
    pub const fn new(numerator: NonZeroU32, denominator: NonZeroU32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Exactly wall clock.
    pub const fn realtime() -> Self {
        Self::new(nz::u32!(1), nz::u32!(1))
    }

    /// The media time this pace earns over `elapsed` of wall clock.
    pub fn media_for(self, elapsed: Duration) -> Duration {
        let nanos = elapsed
            .as_nanos()
            .saturating_mul(u128::from(self.numerator.get()))
            / u128::from(self.denominator.get());
        crate::domain::duration_from_nanos_saturating(nanos)
    }

    /// The wall clock needed to earn `media` at this pace.
    pub fn wall_for(self, media: Duration) -> Duration {
        let nanos = media
            .as_nanos()
            .saturating_mul(u128::from(self.denominator.get()))
            / u128::from(self.numerator.get());
        crate::domain::duration_from_nanos_saturating(nanos)
    }

    /// Whether this pace is strictly slower than `other`.
    pub fn is_slower_than(self, other: Self) -> bool {
        u64::from(self.numerator.get()) * u64::from(other.denominator.get())
            < u64::from(other.numerator.get()) * u64::from(self.denominator.get())
    }
}

/// The throttle applied to a publisher offering media faster than `pace`.
///
/// Enforcement is backpressure only: exceeding the bucket sleeps, and nothing
/// here ever ends a session. A publisher that is merely fast is not a fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ceiling {
    /// Long-run rate the bucket refills at.
    pub pace: Pace,
    /// Media time the bucket may hold, which is the head start a publisher
    /// gets and the size of any burst it may take after running slow.
    ///
    /// Consumable, unlike the standing allowance it replaces: a publisher that
    /// spends it must earn it back at `pace` before bursting again.
    pub burst: Duration,
}

/// The minimum rate a publisher must sustain to stay admitted.
///
/// Where a ceiling throttles, this disconnects: it is how an operator says a
/// stream must be genuinely live rather than merely present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Floor {
    pub pace: Pace,
    /// Averaging window. The first one is startup grace, because a publisher
    /// cannot have sustained any rate before it has run for a window.
    pub window: Duration,
}

/// What a principal is allowed to publish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamPolicy {
    pub takeovers: TakeoverPolicy,
    /// Absent means no rate ceiling: media is packaged as fast as it arrives.
    pub ceiling: Option<Ceiling>,
    /// Absent means no minimum rate.
    pub floor: Option<Floor>,
    /// A forward jump beyond this is a broken timeline rather than media to
    /// wait for. Enforced whether or not a ceiling is configured, because a
    /// jump inflates the timeline regardless of who is pacing it.
    pub maximum_timestamp_jump: Duration,
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
            // No ceiling: a publisher offering media as fast as its link
            // allows is taken to be asking for exactly that.
            ceiling: None,
            floor: None,
            maximum_timestamp_jump: Duration::from_secs(10),
            accepted_video_codecs: vec![Codec::H264, Codec::Hevc, Codec::Av1],
            accepted_audio_codecs: vec![Codec::Aac, Codec::Opus],
            accepted_subtitle_codecs: vec![Codec::WebVtt, Codec::SubRip, Codec::Text],
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
