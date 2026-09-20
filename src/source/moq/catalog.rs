//! The slice of the hang catalog this origin ingests.
//!
//! Owned here rather than taken from the `hang` crate. What ingest reads is a
//! dozen fields of one JSON document, while the crate defining them carries a
//! codec-string parser, a hex codec, and a JSON-merge-patch stack this origin
//! never reaches — and it tracks a moving draft, so depending on it would mean
//! taking churn for fields that are refused here anyway.
//!
//! Parsing is permissive about what it ignores and strict about what it uses.
//! Every optional field defaults, so a publisher adding a property does not
//! fail the catalog; a rendition this origin cannot package is then refused by
//! name in [`map`](super::map) rather than silently dropped.

use std::{
    collections::BTreeMap,
    task::{Poll, ready},
};

use bytes::{BufMut, Bytes, BytesMut};
use serde::Deserialize;

use super::ReadError;

/// The track a hang publisher serves its catalog on.
pub const TRACK_NAME: &str = "catalog.json";

/// The priority hang reserves for the catalog, so it preempts media.
const PRIORITY: u8 = 100;

/// One published catalog.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Catalog {
    pub video: Renditions<VideoConfig>,
    pub audio: Renditions<AudioConfig>,
    #[serde(skip)]
    pub wire_bytes: usize,
}

/// A rendition map, keyed by the track name its media is published on.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Renditions<T> {
    #[serde(default = "BTreeMap::new")]
    pub renditions: BTreeMap<String, T>,
}

// Derived `Default` would require `T: Default`, which a rendition is not.
impl<T> Default for Renditions<T> {
    fn default() -> Self {
        Self {
            renditions: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoConfig {
    /// Set when the rendition lives on another broadcast, which ingest refuses.
    #[serde(default)]
    pub broadcast: Option<String>,
    /// A WebCodecs registry string, such as `avc1.640028`.
    pub codec: String,
    #[serde(default, deserialize_with = "hex_bytes")]
    pub description: Option<Bytes>,
    #[serde(default)]
    pub coded_width: Option<u32>,
    #[serde(default)]
    pub coded_height: Option<u32>,
    #[serde(default)]
    pub framerate: Option<f64>,
    #[serde(default)]
    pub container: Container,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioConfig {
    #[serde(default)]
    pub broadcast: Option<String>,
    pub codec: String,
    #[serde(default)]
    pub sample_rate: u32,
    // The one field hang does not spell in camel case.
    #[serde(default, rename = "numberOfChannels")]
    pub channel_count: u32,
    #[serde(default, deserialize_with = "hex_bytes")]
    pub description: Option<Bytes>,
    #[serde(default)]
    pub container: Container,
}

/// The frame format a rendition is published in.
///
/// Held as the wire string rather than decoded into variants: this origin
/// accepts exactly one container and has to name the others in its refusal, so
/// a variant per format would only give them somewhere to hide.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    #[serde(default)]
    kind: Option<String>,
}

impl Container {
    /// hang omits `container` for its original format, which is not LOC.
    const DEFAULT_KIND: &'static str = "legacy";
    const LOC: &'static str = "loc";

    pub fn kind(&self) -> &str {
        self.kind.as_deref().unwrap_or(Self::DEFAULT_KIND)
    }

    pub fn is_loc(&self) -> bool {
        self.kind() == Self::LOC
    }

    pub fn is_supported(&self) -> bool {
        self.is_loc() || self.kind() == Self::DEFAULT_KIND
    }
}

/// The video codecs this origin packages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    /// `avc1`: parameter sets belong in the catalog description.
    H264,
    /// `avc3`: parameter sets are in band, which ingest refuses.
    H264Inline,
    H265,
    Av1,
}

/// The audio codecs this origin packages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioCodec {
    Aac,
    Opus,
}

/// Reads a WebCodecs codec string far enough to name the bitstream.
///
/// Only the prefix is read. The profile and level digits after it describe a
/// decoder this origin does not run: what it packages comes from the
/// description record, which is authoritative and parsed for real elsewhere.
pub fn video_codec(codec: &str) -> Option<VideoCodec> {
    if codec.starts_with("avc1.") {
        Some(VideoCodec::H264)
    } else if codec.starts_with("avc3.") {
        Some(VideoCodec::H264Inline)
    } else if codec.starts_with("hvc1.") || codec.starts_with("hev1.") {
        Some(VideoCodec::H265)
    } else if codec.starts_with("av01.") {
        Some(VideoCodec::Av1)
    } else {
        None
    }
}

pub fn audio_codec(codec: &str) -> Option<AudioCodec> {
    if codec.starts_with("mp4a.40.") {
        Some(AudioCodec::Aac)
    } else if codec == "opus" {
        Some(AudioCodec::Opus)
    } else {
        None
    }
}

/// Reads catalog versions from the track a publisher serves them on.
pub struct Reader {
    subscriber: moq_net::track::Subscriber,
    maximum_bytes: usize,
}

impl Reader {
    pub fn new(subscriber: moq_net::track::Subscriber, maximum_bytes: usize) -> Self {
        Self {
            subscriber,
            maximum_bytes,
        }
    }

    /// What to ask for when subscribing to the catalog track.
    pub fn subscription() -> moq_net::track::Subscription {
        moq_net::track::Subscription::default().with_priority(PRIORITY)
    }

    /// The next catalog, `None` once the publisher finishes the track.
    ///
    /// One group is one catalog version, so its first frame is the whole
    /// document and the rest of the group — if a publisher ever writes one — is
    /// not part of it.
    pub fn poll_next(&mut self, waiter: &kio::Waiter) -> Poll<Result<Option<Catalog>, ReadError>> {
        let Some(frame) = ready!(self.subscriber.poll_read_frame(waiter))? else {
            return Poll::Ready(Ok(None));
        };
        if frame.payload.len() > self.maximum_bytes {
            return Poll::Ready(Err(ReadError::Malformed(
                "MOQ catalog exceeds the packet byte limit".into(),
            )));
        }
        let mut catalog: Catalog = serde_json::from_slice(&frame.payload).map_err(|error| {
            ReadError::Malformed(format!("the hang catalog is not valid JSON: {error}").into())
        })?;
        catalog.wire_bytes = frame.payload.len();
        Poll::Ready(Ok(Some(catalog)))
    }
}

/// hang hex-encodes `description`; decoder configuration records are small.
fn hex_bytes<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Bytes>, D::Error> {
    let Some(text) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    decode_hex(&text)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("a catalog description is not hex"))
}

fn decode_hex(text: &str) -> Option<Bytes> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = BytesMut::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks_exact(2) {
        out.put_u8((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?);
    }
    Some(out.freeze())
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Hex-encodes bytes the way a publisher's `description` arrives.
///
/// Lives beside the decoder so the two stay each other's inverse, and is
/// shared by the catalog tests in this module's siblings.
#[cfg(test)]
pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}
