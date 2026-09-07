//! Annex-B H.264 from Hang avc3 renditions, normalized for the shared packager.
use super::loc::Frame;
use crate::{
    domain::{DiscoveredTrack, MediaParameters, Payload},
    source::SourceError,
};
use broadcast_common::Parse;
use bytes::Bytes;

#[derive(Default)]
pub struct Inline {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Inline {
    /// Freeze the first parameter sets; changing an in-band config has the same
    /// meaning as changing a catalog description after discovery.
    pub fn convert(
        &mut self,
        frame: &mut Frame,
        track_id: crate::domain::TrackId,
        limit: usize,
    ) -> Result<(), SourceError> {
        if frame.payload.len() > limit {
            return Err(SourceError::PacketPayloadTooLarge {
                limit,
                found: frame.payload.len(),
            });
        }
        let mut payload = Vec::new();
        for nal in transmux::iter_annexb_nals(&frame.payload) {
            let target = match nal[0] & 0x1f {
                7 => Some(&mut self.sps),
                8 => Some(&mut self.pps),
                _ => None,
            };
            if let Some(target) = target {
                if target.as_deref().is_some_and(|previous| previous != nal) {
                    return Err(SourceError::CodecParametersChanged { track_id });
                }
                if target.is_none() {
                    *target = Some(nal.to_vec());
                }
                continue;
            }
            let length = u32::try_from(nal.len())
                .map_err(|_| SourceError::Demux("H.264 NAL too large".into()))?;
            if payload.len().saturating_add(4).saturating_add(nal.len()) > limit {
                return Err(SourceError::PacketPayloadTooLarge {
                    limit,
                    found: payload.len().saturating_add(4).saturating_add(nal.len()),
                });
            }
            payload.extend_from_slice(&length.to_be_bytes());
            payload.extend_from_slice(nal);
        }
        if payload.is_empty() {
            return Err(SourceError::Demux("empty avc3 access unit".into()));
        }
        frame.payload = Bytes::from(payload);
        Ok(())
    }
    /// Install the first in-band decoder configuration before discovery freezes.
    pub fn configure(&self, track: &mut DiscoveredTrack) -> Result<(), SourceError> {
        if track.codec_extradata.is_empty() {
            let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
                return Err(SourceError::Demux(
                    "avc3 first group needs SPS and PPS".into(),
                ));
            };
            if sps.len() < 4 {
                return Err(SourceError::Demux("truncated H.264 SPS".into()));
            }
            let mut config = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
            for (index, nal) in [sps, pps].into_iter().enumerate() {
                if index == 1 {
                    config.push(1);
                }
                let length = u16::try_from(nal.len())
                    .map_err(|_| SourceError::Demux("H.264 parameter set too large".into()))?;
                config.extend_from_slice(&length.to_be_bytes());
                config.extend_from_slice(nal);
            }
            let record = transmux::AVCDecoderConfigurationRecord::parse(&config)
                .map_err(|error| SourceError::Demux(error.to_string().into()))?;
            let info = record.sps[0]
                .decode()
                .map_err(|error| SourceError::Demux(error.to_string().into()))?;
            if let MediaParameters::Video {
                width,
                height,
                video_delay,
                frame_rate,
            } = &mut track.parameters
            {
                *width = std::num::NonZeroU32::new(info.width)
                    .ok_or_else(|| SourceError::Demux("zero H.264 width".into()))?;
                *height = std::num::NonZeroU32::new(info.height)
                    .ok_or_else(|| SourceError::Demux("zero H.264 height".into()))?;
                *frame_rate = crate::media::video_config::h264_frame_rate(
                    info.num_units_in_tick,
                    info.time_scale,
                );
                *video_delay =
                    crate::media::video_config::properties(track.codec, &config).reorder_depth;
            }
            track.codec_extradata = Payload::from_bytes(Bytes::from(config));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{catalog, map};
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn inline_parameter_sets_configure_cmaf_and_changes_are_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let config: catalog::VideoConfig = serde_json::from_value(serde_json::json!({
            "codec": "avc3.64000a", "codedWidth": 16, "codedHeight": 16
        }))?;
        let mut mapped = map::tracks_from_catalog(
            &BTreeMap::from([("video".into(), config)]),
            &BTreeMap::new(),
        )?;
        let record =
            transmux::AVCDecoderConfigurationRecord::parse(crate::mux::fixtures::H264_EXTRADATA)?;
        let mut annexb = Vec::new();
        for nal in [&record.sps[0].0[..], &record.pps[0].0[..], &[0x65, 0x88]] {
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(nal);
        }
        let mut frame = Frame {
            timestamp: moq_net::Timestamp::ZERO,
            payload: Bytes::from(annexb.clone()),
            keyframe: true,
        };
        let mut inline = Inline::default();
        let track = &mut mapped.tracks[0];
        inline.convert(&mut frame, track.id, 1_000_000)?;
        inline.configure(track)?;
        assert!(!track.codec_extradata.is_empty());
        assert_eq!(frame.payload.as_ref(), &[0, 0, 0, 2, 0x65, 0x88]);
        // A later SPS change must not silently reuse the original init segment.
        annexb[5] ^= 1;
        frame.payload = Bytes::from(annexb);
        assert!(matches!(
            inline.convert(&mut frame, track.id, 1_000_000),
            Err(SourceError::CodecParametersChanged { .. })
        ));
        Ok(())
    }
}
