//! Descriptive RTMP metadata applied once, when discovery freezes the catalog.

use crate::domain::{DiscoveredTrack, MediaKind};
use rtmpx::ParsedMetadata;
use rtmpx::amf0::{Amf0Object, Amf0Value};

pub fn apply(track: &mut DiscoveredTrack, metadata: &ParsedMetadata) {
    let (tracks, language, title) = match track.kind() {
        MediaKind::Audio => (&metadata.audio_tracks, "audiolanguage", "audiotitle"),
        MediaKind::Video => (&metadata.video_tracks, "videolanguage", "videotitle"),
        MediaKind::Subtitle => {
            // Script captions have no Enhanced RTMP track-id map. Their
            // language is the publication-wide language supplied by the encoder.
            track.language = text(&metadata.properties, "language");
            return;
        }
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
    use rtmpx::sessions::{DataMessage, DataMessageType};
    use rtmpx::time::RtmpTimestamp;
    use rtmpx::{EnhancedValidationMode, MediaInterpretation, ValidatedMetadata};

    #[test]
    fn per_track_metadata_overrides_publication_defaults() {
        // TrackMetadata is sealed inside rtmpx, so build the onMetaData bytes
        // an encoder would send and parse them like the ingest path does.
        let track = Amf0Object::from([
            ("language".into(), Amf0Value::Utf8String("spa".into())),
            ("title".into(), Amf0Value::Utf8String(" Español ".into())),
        ]);
        let properties = Amf0Object::from([
            ("language".into(), Amf0Value::Utf8String("eng".into())),
            (
                "audiotitle".into(),
                Amf0Value::Utf8String("Main audio".into()),
            ),
            (
                "audioTrackIdInfoMap".into(),
                Amf0Value::Object(Amf0Object::from([("2".into(), Amf0Value::Object(track))])),
            ),
        ]);
        let payload = rtmpx::amf0::serialize(&[
            Amf0Value::Utf8String("onMetaData".into()),
            Amf0Value::Object(properties),
        ])
        .expect("test metadata serializes");
        let message: DataMessage =
            DataMessage::new(DataMessageType::Amf0, RtmpTimestamp::new(0), payload.into());
        let metadata = ValidatedMetadata::parse(message, EnhancedValidationMode::Strict)
            .expect("test metadata validates")
            .into_parts()
            .1;
        let MediaInterpretation::Parsed(metadata) = metadata else {
            panic!("test metadata parses");
        };
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
