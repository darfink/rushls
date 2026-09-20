//! GStreamer AV1G private-PES mapping. This is not a generic AV1 TS mapping.
use crate::{
    domain::{Codec, DiscoveredTrack, MediaParameters, Payload, SourceTrackKey, Timebase, TrackId},
    source::SourceError,
};
use broadcast_common::{Parse, Serialize};
use transmux::{CodecConfig, TrackSpec};

pub fn configuration(
    spec: &TrackSpec,
) -> Result<Option<transmux::Av1ConfigurationBox>, SourceError> {
    let CodecConfig::Data {
        stream_type: 6,
        descriptors,
        ..
    } = &spec.config
    else {
        return Ok(None);
    };
    let mut bytes = &descriptors[..];
    let mut registered = false;
    let mut config = None;
    while bytes.len() >= 2 {
        let size = usize::from(bytes[1]);
        let Some(body) = bytes.get(2..2 + size) else {
            return Err(error("truncated AV1G descriptor"));
        };
        if bytes[0] == 5 && body.starts_with(b"AV1G") {
            registered = true;
        }
        if bytes[0] == 0x80 {
            config = Some(body);
        }
        bytes = &bytes[2 + size..];
    }
    if !registered {
        return Ok(None);
    }
    let config = transmux::Av1ConfigurationBox::parse(
        config.ok_or_else(|| error("AV1G has no av1C descriptor"))?,
    )
    .map_err(|e| error(&e.to_string()))?;
    Ok(Some(config))
}

pub fn track(
    spec: &TrackSpec,
    payload: &[u8],
) -> Result<Option<(DiscoveredTrack, bool)>, SourceError> {
    let Some(sequence) = crate::media::av1::sequence(payload) else {
        return Ok(None);
    };
    let mut config = configuration(spec)?.ok_or_else(|| error("not an AV1G stream"))?;
    // The PMT carries only the four-byte av1C prefix. Config OBUs must include
    // the sequence header for the CMAF decoder to initialize independently.
    let mut cursor = std::io::Cursor::new(payload);
    loop {
        let start =
            usize::try_from(cursor.position()).map_err(|_| error("AV1 OBU offset overflow"))?;
        let header =
            scuffle_av1::ObuHeader::parse(&mut cursor).map_err(|e| error(&e.to_string()))?;
        let end = cursor
            .position()
            .checked_add(
                header
                    .size
                    .ok_or_else(|| error("AV1G requires sized OBUs"))?,
            )
            .ok_or_else(|| error("AV1 OBU size overflow"))?;
        if header.obu_type == scuffle_av1::ObuType::SequenceHeader {
            config.config_obus = payload
                .get(start..usize::try_from(end).map_err(|_| error("AV1 OBU size overflow"))?)
                .ok_or_else(|| error("truncated AV1 sequence"))?
                .to_vec();
            break;
        }
        cursor.set_position(end);
    }
    let track = DiscoveredTrack {
        decoder_config_origin: crate::domain::DecoderConfigOrigin::Synthesized,
        video_cadence: crate::domain::VideoCadence::Unknown,
        id: TrackId(spec.track_id),
        source_key: spec
            .source_pid
            .map(|pid| SourceTrackKey::new(format!("pid/{pid}"))),
        codec: Codec::Av1,
        parameters: MediaParameters::Video {
            width: std::num::NonZeroU32::new(
                u32::try_from(sequence.max_frame_width).map_err(|_| error("AV1 width overflow"))?,
            )
            .ok_or_else(|| error("zero AV1 width"))?,
            height: std::num::NonZeroU32::new(
                u32::try_from(sequence.max_frame_height)
                    .map_err(|_| error("AV1 height overflow"))?,
            )
            .ok_or_else(|| error("zero AV1 height"))?,
            frame_rate: None,
            video_delay: 0,
        },
        timebase: Timebase::hz90k(),
        first_pts: None,
        title: None,
        language: super::map::language_from_es_info(&spec.es_info_descriptors),
        codec_extradata: Payload::from(config.to_bytes()),
    };
    Ok(Some((track, sequence.reduced_still_picture_header)))
}
fn error(message: &str) -> SourceError {
    SourceError::Demux(message.into())
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    #[test]
    fn av1g_in_band_sequence_retains_fixed_cadence() -> Result<(), SourceError> {
        let config = crate::media::fixtures::AV1_FIXED_CADENCE;
        let mut descriptors = vec![5, 4, b'A', b'V', b'1', b'G', 0x80, 4];
        descriptors.extend_from_slice(&config[..4]);
        let spec = TrackSpec::new(
            1,
            90000,
            CodecConfig::Data {
                stream_type: 6,
                descriptors,
                carriage: transmux::ir::DataCarriage::Pes,
            },
        );
        let (track, _) = track(&spec, &config[4..])?.expect("sequence header available");
        assert!(matches!(
            crate::media::cadence::inspect(&track),
            crate::domain::VideoCadence::Fixed { .. }
        ));
        Ok(())
    }
}
