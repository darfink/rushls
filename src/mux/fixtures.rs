//! Packaged rendition and presentation fixtures.
//!
//! The defaults describe a pass-through CMAF publication of the fixtures in
//! [`media::fixtures`](crate::media::fixtures): one output per input track,
//! six-second segments cut into one-second chunks. Tests that care about
//! reconnect matching override the [`RenditionKey`]; tests that care about
//! packaging mode override the chunk target; nothing else has to be spelled.

use std::{num::NonZero, sync::Arc, time::SystemTime};

use crate::{
    domain::{
        DiscoveredTrack, FrameRate, MediaKind, TickDuration, Timebase, TrackId, fixtures as domain,
        rfc6381,
    },
    media::PresentationPlan,
};

use super::{
    MediaSegmentFormat, PackagedPresentation, PackagedRendition, PackagingRenditionId,
    RenditionConfig, RenditionKey, RenditionMedia,
};

/// Segment and chunk targets in the rendition's own timebase.
///
/// `chunk_target` of `None` selects segment-only packaging, which the store
/// treats as a different contract rather than a tuning knob.
pub fn config(
    timebase: Timebase,
    segment_target: TickDuration,
    chunk_target: Option<TickDuration>,
) -> RenditionConfig {
    RenditionConfig {
        timebase,
        segment_target: NonZero::new(segment_target).expect("a segment target is nonzero"),
        chunk_target: chunk_target
            .map(|target| NonZero::new(target).expect("a chunk target is nonzero")),
        segment_format: MediaSegmentFormat::Cmaf,
    }
}

/// The advertised output shape for a test rendition of each kind.
pub fn media(kind: MediaKind) -> RenditionMedia {
    match kind {
        MediaKind::Audio => RenditionMedia::Audio {
            sample_rate: nz::u32!(48_000),
            channels: nz::u16!(2),
        },
        MediaKind::Subtitle => RenditionMedia::Subtitle,
        MediaKind::Video => RenditionMedia::Video {
            width: nz::u32!(1920),
            height: nz::u32!(1080),
            frame_rate: Some(FrameRate::new(nz::u32!(30), nz::u32!(1))),
            video_range: None,
        },
    }
}

/// The RFC 6381 string for a fixture track of `kind` carrying no extradata.
///
/// Derived rather than hardcoded so a change to
/// [`domain::fixtures::codec`](crate::domain::fixtures::codec) cannot leave the
/// declared codec and the advertised string disagreeing.
pub fn codecs(kind: MediaKind) -> Arc<str> {
    rfc6381(domain::codec(kind), None).expect("fixture codecs are representable")
}

/// Builds one packaged rendition, overriding only what a test cares about.
#[derive(Clone, Debug)]
pub struct RenditionBuilder(PackagedRendition);

impl RenditionBuilder {
    /// A 90 kHz output of `kind` with six-second segments and one-second chunks.
    pub fn new(packaging_rendition_id: u32, kind: MediaKind) -> Self {
        Self(PackagedRendition {
            packaging_rendition_id: PackagingRenditionId(packaging_rendition_id),
            key: RenditionKey::new(format!("{kind:?}/{packaging_rendition_id}")),
            source_tracks: Arc::from([TrackId(0)]),
            config: config(Timebase::hz90k(), 6 * 90_000, Some(90_000)),
            media: media(kind),
            codecs: codecs(kind),
            name: Arc::from(format!("{kind:?} {packaging_rendition_id}")),
            language: None,
            is_default: false,
            declared_bandwidth: None,
        })
    }

    /// A pass-through output of one discovered track.
    ///
    /// This is what a real pass-through muxer does, spelled once: take the
    /// track's timebase, name it as the source, and derive the RFC 6381 string
    /// from the codec *and its configuration bytes* rather than from a guess
    /// keyed on media kind. A test that gives its track real extradata gets the
    /// refined `avc1.640028` form out of this without doing anything else.
    pub fn for_track(packaging_rendition_id: u32, track: &DiscoveredTrack) -> Self {
        let mut builder = Self::new(packaging_rendition_id, track.kind())
            .key(&RenditionKey::for_source(track).0)
            .source_tracks(&[track.id.0]);
        builder.0.config.timebase = track.timebase;
        builder.0.language = track.language.as_deref().map(Arc::from);
        if let Some(codecs) = track.rfc6381_codec() {
            builder.0.codecs = codecs;
        }
        builder
    }

    pub fn key(mut self, key: &str) -> Self {
        self.0.key = RenditionKey::new(key);
        self
    }

    pub fn source_tracks(mut self, tracks: &[u32]) -> Self {
        self.0.source_tracks = tracks.iter().copied().map(TrackId).collect();
        self
    }

    pub fn config(mut self, config: RenditionConfig) -> Self {
        self.0.config = config;
        self
    }

    /// Switches between chunked and segment-only packaging, keeping the
    /// rendition's timebase and segment target.
    pub fn chunked(mut self, chunked: bool) -> Self {
        let chunk_target = self.0.config.chunk_target.or(NonZero::new(90_000));
        self.0.config.chunk_target = chunked.then_some(chunk_target).flatten();
        self
    }

    pub fn is_default(mut self, is_default: bool) -> Self {
        self.0.is_default = is_default;
        self
    }

    pub fn build(self) -> PackagedRendition {
        self.0
    }
}

/// A 90 kHz video output whose key is stable across publications.
pub fn video_rendition(packaging_rendition_id: u32) -> PackagedRendition {
    RenditionBuilder::new(packaging_rendition_id, MediaKind::Video).build()
}

/// The default pass-through topology over `renditions`, anchored at the epoch.
pub fn presentation(
    input: &PresentationPlan,
    renditions: Vec<PackagedRendition>,
) -> PackagedPresentation {
    presentation_at(SystemTime::UNIX_EPOCH, input, renditions)
}

/// As [`presentation`], with an explicit wall-clock origin.
///
/// Separate publications need distinct anchors for the store's PDT bookkeeping
/// to be observable, so that case gets its own entry point rather than an
/// `Option` every other caller has to pass.
pub fn presentation_at(
    time_anchor: SystemTime,
    input: &PresentationPlan,
    renditions: Vec<PackagedRendition>,
) -> PackagedPresentation {
    PackagedPresentation::with_default_topology(time_anchor, input, renditions)
        .expect("test packaged presentation is valid")
}
