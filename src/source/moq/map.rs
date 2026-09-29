//! Hang catalog and frames → rushls tracks and packets.
//!
//! LOC and legacy containers carry elementary frames. Other containers fail
//! explicitly so ingest cannot silently discard a rendition. avc3 H.264 gets
//! its decoder configuration from the first access unit; other codecs require
//! catalog configuration (with a mono/stereo fallback for Opus).
//!
//! Text renditions become subtitle tracks. Only hang's `utf8` cues are read:
//! they are the open-ended captions RTMP script data already produces, so the
//! WebVTT path needs nothing new to package them.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use broadcast_common::Parse;
use bytes::Bytes;

use super::{
    catalog::{self, AudioCodec, AudioConfig, TextConfig, VideoCodec, VideoConfig},
    loc,
};

use crate::{
    domain::{Codec, DiscoveredTrack, MediaParameters, Payload, SourceTrackKey, Timebase, TrackId},
    source::{DiscoveryProblem, Packet, SourceError},
};

/// LOC timestamps default to microseconds; every mapped packet uses this clock.
pub const TIMEBASE: Timebase = Timebase::new(nz::u32!(1), nz::u32!(1_000_000));

/// Names and codec bytes frozen at discovery. A later catalog that adds,
/// removes, or reconfigures a rendition is a different publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogFingerprint {
    tracks: BTreeMap<SourceTrackKey, FrozenTrack>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenTrack {
    id: TrackId,
    codec: Codec,
    extradata: Vec<u8>,
    container: String,
    parameters: MediaParameters,
    description_pending: bool,
}

impl CatalogFingerprint {
    pub fn diff(&self, other: &Self) -> Result<(), SourceError> {
        if self.tracks.keys().eq(other.tracks.keys()) {
            for (key, frozen) in &self.tracks {
                let later = other.tracks.get(key).expect("keys compared equal");
                if frozen.codec != later.codec
                    || frozen.extradata != later.extradata
                    || frozen.container != later.container
                    || frozen.parameters != later.parameters
                {
                    return Err(SourceError::CodecParametersChanged {
                        track_id: frozen.id,
                    });
                }
            }
            return Ok(());
        }
        Err(SourceError::TrackSetChanged)
    }
}

#[derive(Clone, Debug)]
pub struct MappedCatalog {
    pub tracks: Vec<DiscoveredTrack>,
    pub fingerprint: CatalogFingerprint,
    pub legacy: Vec<bool>,
    pub inline_h264: Vec<bool>,
}

impl MappedCatalog {
    /// Encoders can publish OpusHead only after their first output, which starts
    /// when we subscribe. Until discovery freezes, replace the synthetic header
    /// with that real decoder configuration without losing prefetched timestamps.
    pub fn refine(&mut self, next: Self) -> Result<(), SourceError> {
        if !self
            .fingerprint
            .tracks
            .keys()
            .eq(next.fingerprint.tracks.keys())
        {
            return Err(SourceError::TrackSetChanged);
        }
        for (key, old) in &self.fingerprint.tracks {
            let new = &next.fingerprint.tracks[key];
            // IDs follow video then audio; fingerprint keys are lexicographic.
            let index = self
                .tracks
                .iter()
                .position(|track| track.id == old.id)
                .expect("fingerprint and tracks have the same IDs");
            // Labels are descriptive, not decoder state, so they stay out of
            // the fingerprint: a newer snapshot may rename a track during
            // discovery, and renaming one after freeze changes nothing.
            self.tracks[index]
                .title
                .clone_from(&next.tracks[index].title);
            self.tracks[index]
                .language
                .clone_from(&next.tracks[index].language);
            // Repeated snapshots preserve timestamps and in-band video metadata.
            if old == new {
                continue;
            }
            let compatible = old.codec == Codec::Opus
                && new.codec == old.codec
                && old.description_pending
                && !new.description_pending
                && old.container == new.container
                && match (old.parameters, new.parameters) {
                    (
                        MediaParameters::Audio {
                            sample_rate: a,
                            channels: b,
                            ..
                        },
                        MediaParameters::Audio {
                            sample_rate: c,
                            channels: d,
                            ..
                        },
                    ) => a == c && b == d,
                    _ => false,
                };
            if !compatible {
                return Err(SourceError::CodecParametersChanged { track_id: old.id });
            }
            self.tracks[index] = next.tracks[index].clone();
        }
        self.fingerprint = next.fingerprint;
        Ok(())
    }
}

pub fn tracks_from_catalog(
    video: &BTreeMap<String, VideoConfig>,
    audio: &BTreeMap<String, AudioConfig>,
    text: &BTreeMap<String, TextConfig>,
) -> Result<MappedCatalog, SourceError> {
    // One wire track cannot be two kinds of media: the frames on it would be
    // read twice, as two different things.
    if video.keys().any(|name| audio.contains_key(name))
        || text
            .keys()
            .any(|name| video.contains_key(name) || audio.contains_key(name))
    {
        return Err(SourceError::Demux(
            "audio, video, and text renditions cannot share a MOQ track name".into(),
        ));
    }
    let mut tracks = Vec::new();
    let mut legacy = Vec::new();
    let mut fingerprint = BTreeMap::new();
    let mut next_id = 0_u32;

    for (name, config) in video {
        refuse_foreign_broadcast(name, config.broadcast.is_some())?;
        require_container(name, &config.container)?;
        legacy.push(!config.container.is_loc());
        let key = source_key("video", name);
        let (codec, parameters, extradata) = video_track(name, config)?;
        let id = TrackId(next_id);
        next_id += 1;
        let track = DiscoveredTrack {
            decoder_config_origin: configuration_origin(config.description.as_ref()),
            video_cadence: crate::domain::VideoCadence::Unknown,
            id,
            source_key: Some(key.clone()),
            codec,
            parameters,
            timebase: TIMEBASE,
            first_pts: None,
            title: config.label.as_deref().and_then(crate::domain::track_title),
            language: None,
            codec_extradata: Payload::from_bytes(extradata),
        };
        fingerprint.insert(key, freeze(&track, &config.container, false));
        tracks.push(track);
    }

    for (name, config) in audio {
        refuse_foreign_broadcast(name, config.broadcast.is_some())?;
        require_container(name, &config.container)?;
        legacy.push(!config.container.is_loc());
        let key = source_key("audio", name);
        let (codec, parameters, extradata) = audio_track(name, config)?;
        let id = TrackId(next_id);
        next_id += 1;
        let description_pending =
            codec == Codec::Opus && config.description.as_ref().is_none_or(Bytes::is_empty);
        let track = DiscoveredTrack {
            decoder_config_origin: configuration_origin(config.description.as_ref()),
            video_cadence: crate::domain::VideoCadence::Unknown,
            id,
            source_key: Some(key.clone()),
            codec,
            parameters,
            timebase: TIMEBASE,
            first_pts: None,
            title: config.label.as_deref().and_then(crate::domain::track_title),
            language: None,
            codec_extradata: Payload::from_bytes(extradata),
        };
        fingerprint.insert(key, freeze(&track, &config.container, description_pending));
        tracks.push(track);
    }

    if tracks.is_empty() {
        return Err(SourceError::Demux(
            "the hang catalog announced no audio or video renditions".into(),
        ));
    }

    // After audio and video, so the empty check above still requires a
    // timeline: cues are placed against picture and sound, never alone.
    for (name, config) in text {
        legacy.push(!config.container.is_loc());
        let key = source_key("text", name);
        let track = text_track(name, config, key.clone(), TrackId(next_id))?;
        next_id += 1;
        fingerprint.insert(key, freeze(&track, &config.container, false));
        tracks.push(track);
    }

    Ok(MappedCatalog {
        tracks,
        legacy,
        inline_h264: video
            .values()
            .map(|config| catalog::video_codec(&config.codec) == Some(VideoCodec::H264Inline))
            .chain(std::iter::repeat_n(false, audio.len() + text.len()))
            .collect(),
        fingerprint: CatalogFingerprint {
            tracks: fingerprint,
        },
    })
}

/// What a later catalog must repeat for this track to be the same publication.
fn freeze(
    track: &DiscoveredTrack,
    container: &catalog::Container,
    description_pending: bool,
) -> FrozenTrack {
    FrozenTrack {
        id: track.id,
        codec: track.codec,
        extradata: track.codec_extradata.as_bytes().to_vec(),
        container: container.kind().to_owned(),
        parameters: track.parameters,
        description_pending,
    }
}

pub fn source_key(kind: &str, rendition: &str) -> SourceTrackKey {
    SourceTrackKey::new(format!("{kind}/{rendition}"))
}

pub fn packet(
    track_id: TrackId,
    codec: Codec,
    frame: &loc::Frame,
    maximum_payload: usize,
) -> Result<Packet, SourceError> {
    if frame.payload.len() > maximum_payload {
        return Err(SourceError::PacketPayloadTooLarge {
            limit: maximum_payload,
            found: frame.payload.len(),
        });
    }
    let pts = presentation_ticks(frame.timestamp)?;
    Ok(Packet {
        track_id,
        pts: Some(pts),
        // Neither Hang container carries DTS. Let the shared video normalizer
        // reconstruct decode time instead of treating reordered PTS as DTS.
        dts: None,
        // Opus is variable-duration: the TOC is authoritative even though the
        // Hang frame header carries no duration. Legal Opus durations are exact
        // multiples of 2.5 ms, so they fit this microsecond clock exactly.
        duration: if codec == Codec::Opus {
            Some(
                i64::from(
                    crate::media::opus::packet_samples(&frame.payload)
                        .map_err(SourceError::Demux)?,
                ) * 1_000_000
                    / 48_000,
            )
        } else {
            None
        },
        // Every cue stands alone, wherever it sits in its group.
        random_access: frame.keyframe || codec == Codec::Text,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: Payload::from_bytes(frame.payload.clone()),
    })
}

fn presentation_ticks(timestamp: moq_net::Timestamp) -> Result<i64, SourceError> {
    let converted = timestamp
        .convert(moq_net::Timescale::MICRO)
        .map_err(|_| SourceError::Demux("a LOC timestamp overflowed in microseconds".into()))?;
    i64::try_from(converted.value())
        .map_err(|_| SourceError::Demux("a LOC timestamp does not fit a signed tick".into()))
}

fn require_container(rendition: &str, container: &catalog::Container) -> Result<(), SourceError> {
    if container.is_supported() {
        return Ok(());
    }
    Err(SourceError::Demux(
        format!(
            "rendition `{rendition}` uses the {} container; only LOC and legacy are accepted",
            container.kind()
        )
        .into(),
    ))
}

fn refuse_foreign_broadcast(rendition: &str, foreign: bool) -> Result<(), SourceError> {
    if foreign {
        return Err(SourceError::Demux(
            format!(
                "rendition `{rendition}` references another broadcast; ingest requires every \
                 track on this publication"
            )
            .into(),
        ));
    }
    Ok(())
}

/// A `utf8` caption rendition: open-ended cues on the shared microsecond clock.
fn text_track(
    name: &str,
    config: &TextConfig,
    key: SourceTrackKey,
    id: TrackId,
) -> Result<DiscoveredTrack, SourceError> {
    refuse_foreign_broadcast(name, config.broadcast.is_some())?;
    require_container(name, &config.container)?;
    require_utf8_text(name, config)?;
    Ok(DiscoveredTrack {
        decoder_config_origin: crate::domain::DecoderConfigOrigin::Publisher,
        video_cadence: crate::domain::VideoCadence::Unknown,
        id,
        source_key: Some(key),
        codec: Codec::Text,
        parameters: MediaParameters::Subtitle,
        timebase: TIMEBASE,
        first_pts: None,
        title: config.label.as_deref().and_then(crate::domain::track_title),
        language: config.lang.clone(),
        codec_extradata: Payload::from(Vec::new()),
    })
}

/// `vtt` and `ttml` cues carry their own timing and markup, which would need a
/// parser per format; refusing them by name keeps a publisher from losing its
/// subtitles without being told.
fn require_utf8_text(rendition: &str, config: &TextConfig) -> Result<(), SourceError> {
    if config.format == catalog::UTF8_TEXT {
        return Ok(());
    }
    Err(SourceError::Demux(
        format!(
            "text rendition `{rendition}` uses the {} format; only utf8 cues are ingested",
            config.format
        )
        .into(),
    ))
}

fn video_track(
    name: &str,
    config: &VideoConfig,
) -> Result<(Codec, MediaParameters, Bytes), SourceError> {
    let codec = match catalog::video_codec(&config.codec) {
        Some(VideoCodec::H264 | VideoCodec::H264Inline) => Codec::H264,
        Some(VideoCodec::H265) => Codec::Hevc,
        Some(VideoCodec::Av1) => Codec::Av1,
        None => {
            return Err(SourceError::Demux(
                format!(
                    "rendition `{name}` uses video codec {}, which this origin does not ingest",
                    config.codec
                )
                .into(),
            ));
        }
    };
    if catalog::video_codec(&config.codec) == Some(VideoCodec::H264Inline) {
        return Ok((codec, video_parameters(codec, &[], config)?, Bytes::new()));
    }
    let extradata = config
        .description
        .clone()
        .filter(|bytes| !bytes.is_empty())
        .ok_or_else(|| {
            SourceError::Demux(
            format!(
                "rendition `{name}` has no decoder configuration; H.264/HEVC/AV1 ingest requires \
                 catalog `description`"
            )
            .into(),
        )
        })?;
    let mut parameters = video_parameters(codec, &extradata, config)?;
    if let MediaParameters::Video {
        video_delay,
        frame_rate,
        ..
    } = &mut parameters
    {
        *frame_rate = frame_rate.or_else(|| {
            config
                .framerate
                .and_then(crate::media::cadence::nominal_rate)
        });
        *video_delay = crate::media::video_config::properties(codec, &extradata).reorder_depth;
    }
    Ok((codec, parameters, extradata))
}

fn video_parameters(
    codec: Codec,
    extradata: &[u8],
    config: &VideoConfig,
) -> Result<MediaParameters, SourceError> {
    if codec == Codec::H264
        && let Ok(record) = transmux::AVCDecoderConfigurationRecord::parse(extradata)
        && let Some(sps) = record.sps.first()
        && let Ok(info) = sps.decode()
    {
        let mut parameters = video_size(info.width, info.height)?;
        if let MediaParameters::Video { frame_rate, .. } = &mut parameters {
            *frame_rate = crate::media::video_config::h264_frame_rate(
                info.num_units_in_tick,
                info.time_scale,
            );
        }
        return Ok(parameters);
    }
    if codec == Codec::Hevc
        && let Ok(record) = transmux::HEVCDecoderConfigurationRecord::parse(extradata)
    {
        for array in &record.arrays {
            for nalu in &array.nalus {
                if let Ok(Some(info)) = nalu.decode_sps() {
                    let mut parameters = video_size(info.width, info.height)?;
                    if let MediaParameters::Video { frame_rate, .. } = &mut parameters {
                        *frame_rate = crate::media::video_config::hevc_frame_rate(
                            info.num_units_in_tick,
                            info.time_scale,
                        );
                    }
                    return Ok(parameters);
                }
            }
        }
    }
    let width = config.coded_width.ok_or(DiscoveryProblem::Missing {
        field: "video width",
    })?;
    let height = config.coded_height.ok_or(DiscoveryProblem::Missing {
        field: "video height",
    })?;
    let mut parameters = video_size(width, height)?;
    if let MediaParameters::Video { frame_rate, .. } = &mut parameters {
        *frame_rate = config
            .framerate
            .and_then(crate::media::cadence::nominal_rate);
    }
    Ok(parameters)
}

fn video_size(width: u32, height: u32) -> Result<MediaParameters, SourceError> {
    Ok(MediaParameters::Video {
        width: nonzero_u32(width, "video width")?,
        height: nonzero_u32(height, "video height")?,
        frame_rate: None,
        video_delay: 0,
    })
}

fn audio_track(
    name: &str,
    config: &AudioConfig,
) -> Result<(Codec, MediaParameters, Bytes), SourceError> {
    match catalog::audio_codec(&config.codec) {
        Some(AudioCodec::Aac) => {
            let extradata = config
                .description
                .clone()
                .filter(|bytes| !bytes.is_empty())
                .ok_or_else(|| {
                    SourceError::Demux(
                        format!(
                            "rendition `{name}` is AAC without AudioSpecificConfig in the catalog"
                        )
                        .into(),
                    )
                })?;
            let parameters =
                crate::media::aac::parameters(&extradata).map_err(SourceError::Demux)?;
            Ok((Codec::Aac, parameters, extradata))
        }
        Some(AudioCodec::Opus) => {
            let extradata = match config.description.clone().filter(|bytes| !bytes.is_empty()) {
                Some(bytes) => bytes,
                None => opus_head(config)?,
            };
            let parameters =
                crate::media::opus::parameters(&extradata).map_err(SourceError::Demux)?;
            Ok((Codec::Opus, parameters, extradata))
        }
        None => Err(SourceError::Demux(
            format!(
                "rendition `{name}` uses audio codec {}, which this origin does not ingest",
                config.codec
            )
            .into(),
        )),
    }
}

/// Catalogs often omit OpusHead when sample rate and channels are already
/// present. Synthesize the same stereo/mono family RTMP uses so CMAF still
/// gets a dOps box.
fn opus_head(config: &AudioConfig) -> Result<Bytes, SourceError> {
    let channels =
        u8::try_from(config.channel_count).map_err(|_| DiscoveryProblem::OutOfRange {
            field: "audio channels",
        })?;
    if channels == 0 {
        return Err(DiscoveryProblem::NotPositive {
            field: "audio channels",
        }
        .into());
    }
    let sample_rate = config.sample_rate.to_le_bytes();
    let mut head = Vec::from(*b"OpusHead");
    // No pre-skip was signaled. Inventing encoder delay would trim real audio.
    head.extend_from_slice(&[1, channels, 0, 0]);
    head.extend_from_slice(&sample_rate);
    head.extend_from_slice(&[0, 0, 0]);
    Ok(Bytes::from(head))
}

fn nonzero_u32(value: u32, field: &'static str) -> Result<NonZeroU32, SourceError> {
    NonZeroU32::new(value).ok_or_else(|| DiscoveryProblem::NotPositive { field }.into())
}

fn configuration_origin(description: Option<&Bytes>) -> crate::domain::DecoderConfigOrigin {
    if description.is_some_and(|bytes| !bytes.is_empty()) {
        crate::domain::DecoderConfigOrigin::Publisher
    } else {
        crate::domain::DecoderConfigOrigin::Synthesized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Renditions are built from catalog JSON rather than struct literals, so
    /// these tests cover the wire format this origin now owns as well as the
    /// mapping applied to it.
    fn rendition<T: serde::de::DeserializeOwned>(fields: serde_json::Value) -> T {
        serde_json::from_value(fields).expect("a test rendition parses")
    }

    fn h264_loc(description: Option<Bytes>) -> VideoConfig {
        let mut fields = serde_json::json!({
            "codec": "avc1.64000a",
            "container": { "kind": "loc" },
            "codedWidth": 16,
            "codedHeight": 16,
        });
        if let Some(bytes) = description {
            fields["description"] = catalog::encode_hex(&bytes).into();
        }
        rendition(fields)
    }

    fn opus_loc() -> AudioConfig {
        rendition(serde_json::json!({
            "codec": "opus",
            "container": { "kind": "loc" },
            "sampleRate": 48_000,
            "numberOfChannels": 2,
        }))
    }

    fn loc_frame(micros: u64, payload: Bytes, keyframe: bool) -> loc::Frame {
        loc::Frame {
            timestamp: moq_net::Timestamp::from_micros(micros).expect("a test timestamp fits"),
            payload,
            keyframe,
        }
    }

    #[test]
    fn loc_h264_and_opus_become_tracks() -> Result<(), SourceError> {
        let mut video = BTreeMap::new();
        video.insert(
            "1080p".to_owned(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        );
        let mut audio = BTreeMap::new();
        audio.insert("opus".to_owned(), opus_loc());

        let mapped = tracks_from_catalog(&video, &audio, &BTreeMap::new())?;
        assert_eq!(mapped.tracks.len(), 2);
        assert_eq!(mapped.tracks[0].codec, Codec::H264);
        assert_eq!(
            mapped.tracks[0]
                .source_key
                .as_ref()
                .map(|key| key.0.as_ref()),
            Some("video/1080p")
        );
        assert_eq!(mapped.tracks[1].codec, Codec::Opus);
        assert_eq!(mapped.tracks[0].timebase, TIMEBASE);
        Ok(())
    }

    #[test]
    fn h264_without_avcc_is_refused() {
        let mut video = BTreeMap::new();
        video.insert("1080p".to_owned(), h264_loc(None));
        assert!(tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new()).is_err());
    }

    #[test]
    fn vp8_and_cmaf_are_refused() {
        let vp8: VideoConfig = rendition(serde_json::json!({
            "codec": "vp8",
            "container": { "kind": "loc" },
            "description": "01",
            "codedWidth": 16,
            "codedHeight": 16,
        }));
        let mut video = BTreeMap::new();
        video.insert("vp8".to_owned(), vp8);
        let error = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())
            .expect_err("vp8 is refused");
        assert!(error.to_string().contains("vp8"), "{error}");

        let cmaf: VideoConfig = rendition(serde_json::json!({
            "codec": "avc1.64000a",
            "container": { "kind": "cmaf", "init": "AAEC" },
            "codedWidth": 16,
            "codedHeight": 16,
        }));
        let mut video = BTreeMap::new();
        video.insert("cmaf".to_owned(), cmaf);
        let error = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())
            .expect_err("cmaf is refused");
        assert!(error.to_string().contains("cmaf"), "{error}");
    }

    #[test]
    fn an_absent_container_is_legacy() -> Result<(), SourceError> {
        let legacy: VideoConfig = rendition(serde_json::json!({
            "codec": "avc1.64000a",
            "description": catalog::encode_hex(crate::mux::fixtures::H264_EXTRADATA),
            "codedWidth": 16,
            "codedHeight": 16,
        }));
        let mut video = BTreeMap::new();
        video.insert("legacy".to_owned(), legacy);
        let mapped = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())?;
        assert_eq!(mapped.legacy, vec![true]);
        Ok(())
    }

    #[test]
    fn avc3_waits_for_in_band_parameters() -> Result<(), SourceError> {
        let avc3: VideoConfig = rendition(serde_json::json!({
            "codec": "avc3.64000a",
            "container": { "kind": "loc" },
            "description": "01",
            "codedWidth": 16,
            "codedHeight": 16,
        }));
        let mut video = BTreeMap::new();
        video.insert("avc3".to_owned(), avc3);
        let mapped = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())?;
        assert_eq!(mapped.inline_h264, vec![true]);
        assert!(mapped.tracks[0].codec_extradata.is_empty());
        Ok(())
    }

    #[test]
    fn a_later_rendition_is_a_track_set_change() -> Result<(), SourceError> {
        let mut video = BTreeMap::new();
        video.insert(
            "1080p".to_owned(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        );
        let first = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())?;
        video.insert(
            "720p".to_owned(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        );
        let later = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())?;
        assert!(matches!(
            first.fingerprint.diff(&later.fingerprint),
            Err(SourceError::TrackSetChanged)
        ));
        Ok(())
    }

    #[test]
    fn loc_frames_use_microsecond_ticks_and_the_keyframe_flag() -> Result<(), SourceError> {
        let frame = loc_frame(1_000, Bytes::from_static(&[1, 2, 3]), true);
        let packet = packet(TrackId(0), Codec::H264, &frame, 64)?;
        assert_eq!(packet.pts, Some(1_000));
        assert_eq!(packet.dts, None);
        assert!(packet.random_access);
        Ok(())
    }

    #[test]
    fn oversized_payloads_are_refused() {
        let frame = loc_frame(0, Bytes::from_static(&[1, 2, 3]), true);
        assert!(matches!(
            packet(TrackId(0), Codec::H264, &frame, 2),
            Err(SourceError::PacketPayloadTooLarge { limit: 2, found: 3 })
        ));
    }
    #[test]
    fn the_same_wire_track_cannot_be_both_audio_and_video() {
        let video = BTreeMap::from([(
            "same".into(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        )]);
        let audio = BTreeMap::from([("same".into(), opus_loc())]);
        assert!(tracks_from_catalog(&video, &audio, &BTreeMap::new()).is_err());
    }

    #[test]
    fn a_container_change_cannot_reuse_the_old_reader() -> Result<(), SourceError> {
        let mut video = BTreeMap::from([(
            "video".into(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        )]);
        let first = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())?;
        video.get_mut("video").expect("track").container = catalog::Container::default();
        let next = tracks_from_catalog(&video, &BTreeMap::new(), &BTreeMap::new())?;
        assert!(matches!(
            first.fingerprint.diff(&next.fingerprint),
            Err(SourceError::CodecParametersChanged { .. })
        ));
        Ok(())
    }

    #[test]
    fn opus_without_a_description_does_not_invent_encoder_delay() -> Result<(), SourceError> {
        let mapped = tracks_from_catalog(
            &BTreeMap::new(),
            &BTreeMap::from([("audio".into(), opus_loc())]),
            &BTreeMap::new(),
        )?;
        let MediaParameters::Audio { timing, .. } = mapped.tracks[0].parameters else {
            panic!("audio");
        };
        assert_eq!(timing.initial_padding_samples, 0);
        assert_eq!(mapped.tracks[0].codec_extradata.len(), 19);
        Ok(())
    }
    #[test]
    fn provisional_opus_can_be_refined_only_once() -> Result<(), SourceError> {
        let video = BTreeMap::new();
        let mut audio = BTreeMap::from([("opus".into(), opus_loc())]);
        let mut mapped = tracks_from_catalog(&video, &audio, &BTreeMap::new())?;
        let provisional = mapped.fingerprint.clone();
        audio.get_mut("opus").expect("audio").description =
            Some(super::super::fixtures::opus_head(312).into());
        let next = tracks_from_catalog(&video, &audio, &BTreeMap::new())?;
        assert!(provisional.diff(&next.fingerprint).is_err());
        mapped.refine(next)?;
        assert_eq!(
            crate::media::opus::configuration(mapped.tracks[0].codec_extradata.as_bytes())
                .map_err(SourceError::Demux)?
                .pre_skip,
            312
        );
        mapped.refine(tracks_from_catalog(&video, &audio, &BTreeMap::new())?)?;
        audio.get_mut("opus").expect("audio").description =
            Some(super::super::fixtures::opus_head(120).into());
        assert!(matches!(
            mapped.refine(tracks_from_catalog(&video, &audio, &BTreeMap::new())?),
            Err(SourceError::CodecParametersChanged { .. })
        ));
        Ok(())
    }

    #[test]
    fn opus_packet_duration_comes_from_each_toc() -> Result<(), SourceError> {
        for (toc, duration) in [(0x80, 2_500), (0x98, 20_000), (0x99, 40_000)] {
            let frame = loc_frame(0, Bytes::from(vec![toc]), true);
            assert_eq!(
                packet(TrackId(0), Codec::Opus, &frame, 64)?.duration,
                Some(duration)
            );
        }
        let invalid = loc_frame(0, Bytes::from_static(&[0x9b]), true);
        assert!(packet(TrackId(0), Codec::Opus, &invalid, 64).is_err());
        Ok(())
    }
    #[test]
    fn refinement_preserves_in_band_video_discovery() -> Result<(), SourceError> {
        let video = BTreeMap::from([(
            "video".into(),
            rendition(serde_json::json!({
                "codec": "avc3.64000a", "codedWidth": 16, "codedHeight": 16,
            })),
        )]);
        let mut audio = BTreeMap::from([("opus".into(), opus_loc())]);
        let mut mapped = tracks_from_catalog(&video, &audio, &BTreeMap::new())?;
        mapped.tracks[0].codec_extradata = crate::mux::fixtures::H264_EXTRADATA.into();
        mapped.tracks[0].first_pts = Some(123);
        let discovered_video = mapped.tracks[0].clone();
        audio.get_mut("opus").expect("audio").description =
            Some(super::super::fixtures::opus_head(312).into());
        mapped.refine(tracks_from_catalog(&video, &audio, &BTreeMap::new())?)?;
        assert_eq!(mapped.tracks[0], discovered_video);
        mapped.refine(tracks_from_catalog(&video, &audio, &BTreeMap::new())?)?;
        assert_eq!(mapped.tracks[0], discovered_video);
        Ok(())
    }

    #[test]
    fn catalog_labels_become_track_titles_and_may_change() -> Result<(), SourceError> {
        let labelled = |label: &str| {
            let mut config = opus_loc();
            config.label = Some(label.to_owned());
            config
        };
        let video = BTreeMap::new();
        let audio = BTreeMap::from([
            ("en".into(), labelled(" English ")),
            // A quote could not be written into a playlist NAME.
            ("sv".into(), labelled("Svenska \"SDH\"")),
        ]);
        let mut mapped = tracks_from_catalog(&video, &audio, &BTreeMap::new())?;
        assert_eq!(mapped.tracks[0].title.as_deref(), Some("English"));
        assert_eq!(mapped.tracks[1].title, None);

        // A later snapshot may rename a track without changing its decoder state.
        let renamed = BTreeMap::from([
            ("en".into(), labelled("English (commentary)")),
            ("sv".into(), labelled("Svenska")),
        ]);
        let next = tracks_from_catalog(&video, &renamed, &BTreeMap::new())?;
        mapped.fingerprint.diff(&next.fingerprint)?;
        mapped.refine(next)?;
        assert_eq!(
            mapped.tracks[0].title.as_deref(),
            Some("English (commentary)")
        );
        assert_eq!(mapped.tracks[1].title.as_deref(), Some("Svenska"));
        Ok(())
    }

    fn utf8_text(fields: serde_json::Value) -> TextConfig {
        let mut base = serde_json::json!({ "format": "utf8", "container": { "kind": "legacy" } });
        if let (Some(base), serde_json::Value::Object(extra)) = (base.as_object_mut(), fields) {
            base.extend(extra);
        }
        rendition(base)
    }

    #[test]
    fn utf8_text_renditions_become_open_ended_subtitle_tracks() -> Result<(), SourceError> {
        let video = BTreeMap::from([(
            "1080p".into(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        )]);
        let text = BTreeMap::from([(
            "captions".into(),
            utf8_text(serde_json::json!({
                "role": "caption", "lang": "en", "label": "English (CC)", "jitter": 50,
            })),
        )]);
        let mapped = tracks_from_catalog(&video, &BTreeMap::new(), &text)?;

        // Text follows audio and video, keyed and clocked like every other track.
        let cues = &mapped.tracks[1];
        assert_eq!(cues.id, TrackId(1));
        assert_eq!(cues.kind(), crate::domain::MediaKind::Subtitle);
        assert_eq!(cues.codec, Codec::Text);
        assert_eq!(cues.timebase, TIMEBASE);
        assert_eq!(cues.source_key, Some(source_key("text", "captions")));
        assert_eq!(cues.title.as_deref(), Some("English (CC)"));
        assert_eq!(cues.language.as_deref(), Some("en"));
        assert_eq!(mapped.legacy, vec![false, true]);
        assert_eq!(mapped.inline_h264, vec![false, false]);

        // A cue is a random access point wherever it sits in its group.
        let frame = loc_frame(5_000, Bytes::from_static(b"hello"), false);
        let cue = packet(cues.id, Codec::Text, &frame, 64)?;
        assert!(cue.random_access);
        assert_eq!((cue.pts, cue.duration), (Some(5_000), None));
        Ok(())
    }

    #[test]
    fn unsupported_or_misplaced_text_renditions_are_refused_by_name() {
        let video = BTreeMap::from([(
            "main".into(),
            h264_loc(Some(Bytes::from_static(
                crate::mux::fixtures::H264_EXTRADATA,
            ))),
        )]);
        for format in ["vtt", "ttml", "srt"] {
            let text = BTreeMap::from([(
                "subs".into(),
                utf8_text(serde_json::json!({ "format": format })),
            )]);
            let error = tracks_from_catalog(&video, &BTreeMap::new(), &text)
                .expect_err("only utf8 is ingested");
            assert!(error.to_string().contains(format), "{error}");
            assert!(error.to_string().contains("subs"), "{error}");
        }

        let clash = BTreeMap::from([("main".into(), utf8_text(serde_json::json!({})))]);
        assert!(tracks_from_catalog(&video, &BTreeMap::new(), &clash).is_err());

        // Cues are placed against picture and sound, so they cannot be alone.
        let alone = BTreeMap::from([("subs".into(), utf8_text(serde_json::json!({})))]);
        assert!(tracks_from_catalog(&BTreeMap::new(), &BTreeMap::new(), &alone).is_err());
    }

    #[test]
    fn catalog_video_preserves_codec_cadence_for_all_supported_codecs()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::media::fixtures::{AV1_FIXED_CADENCE, H264_FIXED_CADENCE, HEVC_FIXED_CADENCE};
        for (codec, bytes) in [
            ("avc1.64000a", H264_FIXED_CADENCE),
            ("hvc1.1.6.L60.90", HEVC_FIXED_CADENCE),
            ("av01.0.00M.08", AV1_FIXED_CADENCE),
        ] {
            let mut config: VideoConfig = serde_json::from_value(
                serde_json::json!({"codec":codec,"codedWidth":64,"codedHeight":64,"framerate":30.0}),
            )?;
            config.description = Some(Bytes::from_static(bytes));
            let mapped = tracks_from_catalog(
                &BTreeMap::from([("video".into(), config)]),
                &BTreeMap::new(),
                &BTreeMap::new(),
            )?;
            let cadence = crate::media::cadence::inspect(&mapped.tracks[0]);
            assert!(
                matches!(cadence, crate::domain::VideoCadence::Fixed { .. }),
                "{codec}: {cadence:?}"
            );
            assert_eq!(
                cadence.rate(),
                Some(crate::domain::FrameRate::new(nz::u32!(25), nz::u32!(1)))
            );
        }
        Ok(())
    }
}
