//! Who may publish, as what stream, under which constraints.
//!
//! [`StreamPolicy`] lives here rather than in `media` because it is an
//! authorization decision that happens to be expressed in media terms. Media
//! validation consumes the policy but never learns what a grant is, which keeps
//! the two modules from depending on each other.

use std::{
    num::{NonZeroU16, NonZeroU32},
    time::Duration,
};

use derive_more::{Debug, Display};
use thiserror::Error;

use crate::domain::{BoxFuture, Codec, FrameRate, StreamId};

mod http;
mod open;
mod predicate;

pub use http::{HttpAuthConfig, HttpAuthenticator};
pub use open::OpenStreamAuthenticator;
pub use predicate::{Bounds, Codecs, FrameBox, Resolution};

#[cfg(test)]
mod fixtures;

pub use crate::domain::{ClientInfo, IngestProtocol, PublishResource};

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
/// Enforcement is backpressure only: media ahead of its deadline waits, and nothing
/// here ever ends a session. A publisher that is merely fast is not a fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ceiling {
    /// Long-run media rate used to calculate admission deadlines.
    pub pace: Pace,
    /// Media-time head start after pre-roll, and bounded catch-up allowance
    /// after a stalled publisher resumes. Zero still credits processing time
    /// toward the next deadline; it does not add a full interval of sleep.
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

/// Which video a publisher may offer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoAccept {
    pub codecs: Codecs,
    pub resolution: Resolution,
    pub frame_rate: Bounds<FrameRate>,
    pub tracks: Bounds<usize>,
}

/// Which audio a publisher may offer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioAccept {
    pub codecs: Codecs,
    /// Hertz. Compared numerically, never as text, because `"8kHz"` sorts
    /// above `"48kHz"` lexicographically.
    pub sample_rate: Bounds<NonZeroU32>,
    pub channels: Bounds<NonZeroU16>,
    pub tracks: Bounds<usize>,
}

/// Which subtitles a publisher may offer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SubtitleAccept {
    pub codecs: Codecs,
    pub tracks: Bounds<usize>,
}

/// What a principal is allowed to publish.
///
/// The admission controls and the media predicates sit together because they
/// answer one question — may this publisher send this, on these terms — and
/// are the only rules that may act on a live session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamPolicy {
    pub takeovers: TakeoverPolicy,
    /// Absent means no rate ceiling: media is packaged as fast as it arrives.
    pub ceiling: Option<Ceiling>,
    /// Absent means no minimum rate.
    pub floor: Option<Floor>,
    /// Strict enforces declared timing; permissive permits bounded, reported compensation.
    pub input_mode: crate::domain::InputMode,
    pub video: VideoAccept,
    pub audio: AudioAccept,
    pub subtitles: SubtitleAccept,
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
            input_mode: crate::domain::InputMode::Permissive,
            // Codecs are the muxable set rather than `Any`: they name what
            // this origin can package, which is a capability rather than a
            // policy, and a track it cannot mux must be refused at admission
            // instead of failing seconds into the session.
            video: VideoAccept {
                codecs: Codecs::OneOf(vec![Codec::H264, Codec::Hevc, Codec::Av1]),
                ..VideoAccept::default()
            },
            audio: AudioAccept {
                codecs: Codecs::OneOf(vec![Codec::Aac, Codec::Opus, Codec::Flac]),
                ..AudioAccept::default()
            },
            subtitles: SubtitleAccept {
                codecs: Codecs::OneOf(vec![Codec::WebVtt, Codec::SubRip, Codec::Text]),
                ..SubtitleAccept::default()
            },
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
