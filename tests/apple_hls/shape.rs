//! Track topologies the ingest transports cannot express on their own.
//!
//! Some presentations this origin can legitimately be asked to serve have no
//! path through any adapter it currently has. RTMP script-data captions produce
//! at most one subtitle track; MPEG-TS and MOQ produce none at all. A ladder
//! with a subtitle rendition per language is nevertheless something the manifest
//! projection, the WebVTT muxer, and the store all claim to support, and Apple
//! has opinions about how it should look.
//!
//! So the shape of a publication is decided here, above the adapter and below
//! the session: a [`PacketSource`] wrapper rewrites the discovered catalog and
//! the packet stream to match. That keeps the packaging and delivery paths
//! under test honest while the input remains a real demuxed fixture.
//!
//! Nothing here fabricates media. Every packet it emits came out of a real
//! demuxer; a duplicated rendition carries the same access units under a new
//! track id, which is exactly what a second language track would look like to
//! everything downstream of discovery.

use std::sync::Arc;

use rushls::{
    domain::{Appender, BoxFuture, DiscoveredTrack, MediaKind, TrackCatalog, TrackId},
    observe::SourceMeters,
    source::{
        AcceptedPublish, DiscoveryLimits, DiscoveryReport, InputState, Packet, PacketSource,
        PendingPublish, PublishRejection, SourceError, TransportError,
    },
};

use rushls::admission::{PublishGrant, PublishRequest};

/// What to do to a publication's track catalog before the session sees it.
#[derive(Clone, Debug, Default)]
pub struct Shape {
    /// Kinds to drop entirely. Used for audio-only and video-only cases.
    pub drop: Vec<MediaKind>,
    /// Extra language tags to duplicate the first subtitle track under.
    ///
    /// The original keeps its own language; one clone is added per entry.
    pub extra_subtitle_languages: Vec<&'static str>,
    /// Extra language tags to duplicate the first audio track under.
    pub extra_audio_languages: Vec<&'static str>,
    /// Stamp `LANGUAGE` on tracks that arrived without one.
    ///
    /// MPEG-TS already copies PMT ISO 639 onto the catalog. RTMP and caption
    /// tracks still have none, and Apple's reports want a tag when one exists.
    pub label_languages: bool,
}

impl Shape {
    pub fn labelled() -> Self {
        Self {
            label_languages: true,
            ..Self::default()
        }
    }

    pub fn without(mut self, kind: MediaKind) -> Self {
        self.drop.push(kind);
        self
    }

    pub fn subtitle_languages(mut self, languages: &[&'static str]) -> Self {
        self.extra_subtitle_languages.extend_from_slice(languages);
        self
    }

    pub fn audio_languages(mut self, languages: &[&'static str]) -> Self {
        self.extra_audio_languages.extend_from_slice(languages);
        self
    }

    fn is_identity(&self) -> bool {
        self.drop.is_empty()
            && self.extra_subtitle_languages.is_empty()
            && self.extra_audio_languages.is_empty()
            && !self.label_languages
    }
}

/// Wraps a pending publish so the shape applies once discovery has run.
pub fn shaped(inner: Box<dyn PendingPublish>, shape: Shape) -> Box<dyn PendingPublish> {
    Box::new(ShapedPublish { inner, shape })
}

struct ShapedPublish {
    inner: Box<dyn PendingPublish>,
    shape: Shape,
}

impl PendingPublish for ShapedPublish {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        self.inner.publish_request()
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            let mut accepted = self.inner.accept(grant, meters).await?;
            accepted.source = Box::new(ShapedSource {
                inner: accepted.source,
                shape: self.shape,
                clones: Vec::new(),
                dropped: Vec::new(),
            });
            Ok(accepted)
        })
    }

    fn reject(
        self: Box<Self>,
        rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        self.inner.reject(rejection)
    }
}

struct ShapedSource {
    inner: Box<dyn PacketSource>,
    shape: Shape,
    /// `(source track, clone track)` pairs, applied to every packet.
    clones: Vec<(TrackId, TrackId)>,
    dropped: Vec<TrackId>,
}

impl PacketSource for ShapedSource {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            let mut report = self.inner.discover(limits).await?;
            if self.shape.is_identity() {
                return Ok(report);
            }
            let mut tracks: Vec<DiscoveredTrack> = report.tracks.tracks().to_vec();

            self.dropped = tracks
                .iter()
                .filter(|track| self.shape.drop.contains(&track.kind()))
                .map(|track| track.id)
                .collect();
            tracks.retain(|track| !self.shape.drop.contains(&track.kind()));

            // Clone before labelling so a clone's explicit language is not
            // overwritten by the "first audio is English" default below.
            let mut next_id = tracks.iter().map(|track| track.id.0).max().unwrap_or(0) + 1;
            for (kind, languages) in [
                (MediaKind::Subtitle, &self.shape.extra_subtitle_languages),
                (MediaKind::Audio, &self.shape.extra_audio_languages),
            ] {
                let Some(source) = tracks.iter().find(|track| track.kind() == kind).cloned() else {
                    if languages.is_empty() {
                        continue;
                    }
                    return Err(SourceError::Discovery(
                        rushls::source::DiscoveryProblem::Missing {
                            field: "a track to duplicate",
                        },
                    ));
                };
                for language in languages {
                    let id = TrackId(next_id);
                    next_id += 1;
                    self.clones.push((source.id, id));
                    tracks.push(DiscoveredTrack {
                        id,
                        // A duplicate has no distinct identity in the input, and
                        // reusing the original's would collide in every map
                        // keyed by it.
                        source_key: None,
                        language: Some((*language).to_owned()),
                        title: Some(format!("{language} ({kind:?})")),
                        ..source.clone()
                    });
                }
            }

            if self.shape.label_languages {
                label(&mut tracks);
            }
            report.tracks = TrackCatalog::new(tracks)?;
            Ok(report)
        })
    }

    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        Box::pin(async move {
            if self.clones.is_empty() && self.dropped.is_empty() {
                return self.inner.fill(out).await;
            }
            // Buffered because a packet has to be seen before it can be
            // duplicated, and the appender is write-only.
            let mut batch: Vec<Packet> = Vec::new();
            let state = self.inner.fill(&mut batch).await?;
            for packet in batch {
                if self.dropped.contains(&packet.track_id) {
                    continue;
                }
                for (source, clone) in &self.clones {
                    if packet.track_id == *source {
                        out.push(Packet {
                            track_id: *clone,
                            ..packet.clone()
                        });
                    }
                }
                out.push(packet);
            }
            Ok(state)
        })
    }
}

/// Stamps `LANGUAGE` on tracks the adapter left unlabelled.
fn label(tracks: &mut [DiscoveredTrack]) {
    let mut audio = 0_usize;
    for track in tracks {
        if track.language.is_some() {
            continue;
        }
        match track.kind() {
            MediaKind::Audio => {
                track.language = Some(if audio == 0 { "en" } else { "es" }.into());
                audio += 1;
            }
            MediaKind::Subtitle => track.language = Some("en".into()),
            MediaKind::Video => {}
        }
    }
}
