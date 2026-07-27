//! Track and catalog fixtures shared by every layer's tests.
//!
//! These live in `domain` for the same reason the types do: the module depends
//! on nothing above it, so any layer may use it without inverting the graph.
//! Each layer above adds a `fixtures` module for the types it introduces, so a
//! fixture never has to reach upward for something it cannot see.
//!
//! The defaults describe one ordinary contribution feed — 1080p30 H.264 video,
//! 48 kHz stereo AAC audio, WebVTT subtitles, all on a 90 kHz timebase starting
//! at zero. A test overrides only the field it is actually about, which is what
//! makes the interesting value visible at the call site instead of buried in
//! thirty lines of boilerplate.

use super::{
    Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters, Payload, TickTimestamp,
    Timebase, TrackCatalog, TrackId,
};

/// The declared shape of a test track of each kind.
pub fn parameters(kind: MediaKind) -> MediaParameters {
    match kind {
        MediaKind::Audio => MediaParameters::Audio {
            sample_rate: nz::u32!(48_000),
            channels: nz::u16!(2),
            frame_size: Some(nz::u32!(1_024)),
            bit_depth: Some(nz::u16!(16)),
        },
        MediaKind::Subtitle => MediaParameters::Subtitle,
        MediaKind::Video => MediaParameters::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(nz::u32!(30), nz::u32!(1))),
            video_delay: 0,
        },
    }
}

/// The codec a test track of each kind carries unless a test says otherwise.
pub fn codec(kind: MediaKind) -> Codec {
    match kind {
        MediaKind::Audio => Codec::Aac,
        MediaKind::Subtitle => Codec::WebVtt,
        MediaKind::Video => Codec::H264,
    }
}

/// Builds one discovered track, overriding only what a test cares about.
#[derive(Clone, Debug)]
pub struct TrackBuilder(DiscoveredTrack);

impl TrackBuilder {
    pub fn new(id: u32, kind: MediaKind) -> Self {
        Self(DiscoveredTrack {
            id: TrackId(id),
            source_key: None,
            codec: codec(kind),
            parameters: parameters(kind),
            timebase: Timebase::hz90k(),
            first_pts: Some(0),
            title: None,
            language: None,
            codec_extradata: Payload::default(),
        })
    }

    pub fn codec(mut self, codec: Codec) -> Self {
        self.0.codec = codec;
        self
    }

    pub fn parameters(mut self, parameters: MediaParameters) -> Self {
        self.0.parameters = parameters;
        self
    }

    pub fn timebase(mut self, timebase: Timebase) -> Self {
        self.0.timebase = timebase;
        self
    }

    pub fn first_pts(mut self, first_pts: Option<TickTimestamp>) -> Self {
        self.0.first_pts = first_pts;
        self
    }

    pub fn codec_extradata(mut self, codec_extradata: impl Into<Payload>) -> Self {
        self.0.codec_extradata = codec_extradata.into();
        self
    }

    pub fn build(self) -> DiscoveredTrack {
        self.0
    }
}

/// A 90 kHz track of `kind` that starts at zero.
pub fn track(id: u32, kind: MediaKind) -> DiscoveredTrack {
    TrackBuilder::new(id, kind).build()
}

/// A catalog that is expected to be valid; panics if the fixture is not.
pub fn catalog(tracks: Vec<DiscoveredTrack>) -> TrackCatalog {
    TrackCatalog::new(tracks).expect("test catalog is valid")
}

/// The single-video-track catalog most pipeline tests are built on.
pub fn video_catalog() -> TrackCatalog {
    catalog(vec![track(0, MediaKind::Video)])
}
