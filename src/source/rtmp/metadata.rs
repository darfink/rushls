//! Descriptive RTMP metadata applied once, when discovery freezes the catalog.

use crate::domain::{DiscoveredTrack, MediaKind};
use rtmpx::ParsedMetadata;
use rtmpx::amf0::{Amf0Object, Amf0Value};

pub fn apply(track: &mut DiscoveredTrack, metadata: &ParsedMetadata) {
    let (tracks, language, title) = match track.kind() {
        MediaKind::Audio => (&metadata.audio_tracks, "audiolanguage", "audiotitle"),
        MediaKind::Video => (&metadata.video_tracks, "videolanguage", "videotitle"),
        MediaKind::Subtitle => return,
    };
    let numbered = track
        .source_key
        .as_ref()
        .and_then(|key| key.0.split_once('/'))
        .and_then(|(_, id)| id.parse::<u32>().ok())
        .and_then(|id| tracks.get(&id));
    // Track-specific values win. Missing or invalid properties fall back to
    // the media-kind value, then the publication-wide value.
    track.language = numbered
        .and_then(|entry| text(&entry.properties, "language"))
        .or_else(|| text(&metadata.properties, language))
        .or_else(|| text(&metadata.properties, "language"));
    track.title = numbered
        .and_then(|entry| text(&entry.properties, "title"))
        .or_else(|| text(&metadata.properties, title))
        .or_else(|| text(&metadata.properties, "title"));
}

fn text(properties: &Amf0Object, key: &str) -> Option<String> {
    let Amf0Value::Utf8String(value) = properties.get(key)? else {
        return None;
    };
    let value = value.trim();
    // These values can appear in manifests. Ignore control characters and
    // unreasonable lengths rather than copying arbitrary AMF data downstream.
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SourceTrackKey, fixtures::TrackBuilder};
    #[test]
    fn per_track_metadata_overrides_publication_defaults() {
        let mut metadata = ParsedMetadata::default();
        metadata
            .properties
            .insert("language".into(), Amf0Value::Utf8String("eng".into()));
        metadata.properties.insert(
            "audiotitle".into(),
            Amf0Value::Utf8String("Main audio".into()),
        );
        metadata.audio_tracks.insert(
            2,
            rtmpx::metadata::TrackMetadata {
                track_id: 2,
                codec: None,
                properties: Amf0Object::from([
                    ("language".into(), Amf0Value::Utf8String("spa".into())),
                    ("title".into(), Amf0Value::Utf8String(" Español ".into())),
                ]),
            },
        );
        let mut track = TrackBuilder::new(0, MediaKind::Audio).build();
        track.source_key = Some(SourceTrackKey::new("audio/2"));
        apply(&mut track, &metadata);
        assert_eq!(track.language.as_deref(), Some("spa"));
        assert_eq!(track.title.as_deref(), Some("Español"));
        track.source_key = Some(SourceTrackKey::new("audio"));
        apply(&mut track, &metadata);
        assert_eq!(track.language.as_deref(), Some("eng"));
        assert_eq!(track.title.as_deref(), Some("Main audio"));
    }
}
