//! Shared in-process MOQ publication fixtures.
use super::{catalog, source::MoqPacketSource};
use crate::{
    observe::{ProcessMeters, SessionMeters},
    source::{DiscoveryLimits, InputLimits},
};
use moq_net::Timestamp;
pub struct Fixture {
    pub producer: moq_net::broadcast::Producer,
    pub tracks: std::collections::HashMap<String, moq_net::track::Producer>,
    pub catalog: moq_net::track::Producer,
}

impl Fixture {
    pub fn new() -> (Self, MoqPacketSource) {
        let producer = moq_net::broadcast::Info::new().produce();
        let catalog = producer
            .create_track(catalog::TRACK_NAME, moq_net::track::Info::default())
            .expect("catalog track");
        let consumer = producer.consume();
        let meters = SessionMeters::new(ProcessMeters::default());
        let source = MoqPacketSource::from_broadcast(
            consumer,
            InputLimits::permissive(),
            meters.source_view(),
        )
        .expect("source");
        (
            Self {
                producer,
                tracks: std::collections::HashMap::new(),
                catalog,
            },
            source,
        )
    }

    pub fn publish_catalog(&mut self, catalog: &serde_json::Value) {
        let payload = serde_json::to_vec(catalog).expect("catalog serializes");
        let mut group = self.catalog.append_group().expect("catalog group");
        group
            .write_frame(Timestamp::ZERO, payload)
            .expect("catalog frame");
        group.finish().expect("catalog group finishes");
    }

    pub fn track(&mut self, name: &str) -> &mut moq_net::track::Producer {
        if !self.tracks.contains_key(name) {
            let track = self
                .producer
                .create_track(name.to_owned(), None)
                .expect("media track");
            self.tracks.insert(name.to_owned(), track);
        }
        self.tracks.get_mut(name).expect("just inserted")
    }

    /// One LOC frame in a group of its own, which is what makes it the
    /// group's random access point.
    pub fn publish_frame(&mut self, name: &str, micros: u64, payload: &[u8]) {
        let data = moq_loc::encode(micros, payload).expect("a LOC frame encodes");
        let timestamp = Timestamp::from_micros(micros).expect("a test timestamp fits");
        let track = self.track(name);
        let mut group = track.append_group().expect("media group");
        group
            .write_frame(timestamp, data)
            .expect("LOC frame writes");
        group.finish().expect("media group finishes");
    }

    /// One legacy-container frame (a microsecond varint, then the payload) in
    /// a group of its own, which is how hang publishes a caption cue.
    pub fn publish_legacy_frame(&mut self, name: &str, micros: u64, payload: &[u8]) {
        let mut data = bytes::BytesMut::new();
        moq_net::VarInt::from_u64(micros)
            .expect("a test timestamp fits a varint")
            .encode_quic(&mut data)
            .expect("a varint encodes");
        data.extend_from_slice(payload);
        let timestamp = Timestamp::from_micros(micros).expect("a test timestamp fits");
        let track = self.track(name);
        let mut group = track.append_group().expect("media group");
        group
            .write_frame(timestamp, data.freeze())
            .expect("legacy frame writes");
        group.finish().expect("media group finishes");
    }

    pub fn finish_media(&mut self) {
        for (_, producer) in self.tracks.drain() {
            producer.finish().expect("LOC tracks finish");
        }
    }
}

pub fn loc_catalog() -> serde_json::Value {
    serde_json::json!({
        "video": { "renditions": { "1080p": {
            "codec": "avc1.64000a",
            "container": { "kind": "loc" },
            "description": catalog::encode_hex(crate::mux::fixtures::H264_EXTRADATA),
            "codedWidth": 16,
            "codedHeight": 16,
        } } },
        "audio": { "renditions": { "opus": {
            "codec": "opus",
            "container": { "kind": "loc" },
            "sampleRate": 48_000,
            "numberOfChannels": 2,
        } } },
    })
}

/// [`loc_catalog`] plus one legacy `utf8` caption rendition named `captions`.
pub fn captioned_catalog() -> serde_json::Value {
    let mut catalog = loc_catalog();
    catalog["text"] = serde_json::json!({ "renditions": { "captions": {
        "format": "utf8",
        "role": "caption",
        "lang": "en",
        "label": "English",
        "container": { "kind": "legacy" },
    } } });
    catalog
}

pub fn discovery_limits() -> DiscoveryLimits {
    DiscoveryLimits {
        maximum_probe_bytes: 8 * 1024 * 1024,
        maximum_wall_time: std::time::Duration::from_secs(2),
    }
}

/// Chrome supplies this header after the first encoded audio output.
pub fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000_u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    head
}
