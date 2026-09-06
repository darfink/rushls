//! CMAF initialization and media fragments from transmux box builders.
//!
//! The live cut loop stays in [`super::CmafTrack`]. This module only writes
//! `ftyp`/`moov` and `styp`/`moof`/`mdat`, plus the edit list that maps encoder
//! delay and a delayed first decode time onto the presentation timeline.

use std::num::NonZeroU32;

use broadcast_common::{Parse, Serialize};
use bytes::Bytes;
use transmux::{
    AVCDecoderConfigurationRecord, Av1ConfigurationBox, CodecConfig, EditBox, EditListBox,
    EditListEntry, FileTypeBox, FragmentTrackData, HEVCDecoderConfigurationRecord, MovieBox,
    OpusSpecificBox, Sample, TrackSpec, aac_config_from_asc_bytes, build_init_segment,
    build_media_segment,
};

use crate::{
    domain::{
        Codec, DiscoveredTrack, MediaParameters, Payload, TickDuration, TickTimestamp, Timebase,
    },
    media::NormalizedSample,
};

/// ISOBMFF track id for a one-rendition CMAF output. The number is arbitrary
/// as long as init and fragments agree; 1 is the conventional first track.
const TRACK_ID: u32 = 1;

pub(super) struct CmafOutput {
    spec: TrackSpec,
    padding_ticks: u64,
    pending: Vec<PendingSample>,
    first_decode: Option<TickTimestamp>,
    first_pts: Option<TickTimestamp>,
    initialized: bool,
    sequence: u32,
}

struct PendingSample {
    dts: TickTimestamp,
    pts: TickTimestamp,
    duration: TickDuration,
    random_access: bool,
    data: Bytes,
}

impl CmafOutput {
    pub(super) fn open(track: &DiscoveredTrack) -> Result<Self, Box<str>> {
        let timescale = media_timescale(track.timebase)?;
        let config = codec_config(track)?;
        Ok(Self {
            spec: TrackSpec::new(TRACK_ID, timescale, config),
            padding_ticks: padding_ticks(track)?,
            pending: Vec::new(),
            first_decode: None,
            first_pts: None,
            initialized: false,
            sequence: 1,
        })
    }

    pub(super) fn write(
        &mut self,
        sample: &NormalizedSample,
        pts: TickTimestamp,
        dts: TickTimestamp,
    ) {
        if self.first_decode.is_none() {
            self.first_decode = Some(dts);
            self.first_pts = Some(pts);
        }
        let duration = sample.duration();
        self.pending.push(PendingSample {
            dts,
            pts,
            duration,
            random_access: sample.random_access(),
            data: sample_payload(sample).bytes().clone(),
        });
    }

    pub(super) fn flush_fragment(&mut self) -> Result<Payload, Box<str>> {
        if !self.initialized {
            let payload = self.build_init()?;
            self.initialized = true;
            return Ok(payload);
        }
        self.build_media()
    }

    pub(super) fn finalize(&mut self) {
        self.pending.clear();
    }

    fn build_init(&self) -> Result<Payload, Box<str>> {
        let init = build_init_segment(std::slice::from_ref(&self.spec), self.spec.timescale)
            .map_err(mux)?;
        let elst = edit_list(
            self.padding_ticks,
            self.first_decode.unwrap_or(0),
            self.first_pts.unwrap_or(0),
        );
        Ok(Payload::from_bytes(with_cmaf_init(&init, elst)?))
    }

    fn build_media(&mut self) -> Result<Payload, Box<str>> {
        if self.pending.is_empty() {
            return Ok(Payload::default());
        }
        let origin = self.first_decode.unwrap_or(0);
        let mut samples = Vec::with_capacity(self.pending.len());
        for pending in self.pending.drain(..) {
            let dts = pending
                .dts
                .checked_sub(origin)
                .ok_or_else(|| Box::<str>::from("fragment DTS underflowed the track origin"))?;
            let pts = pending
                .pts
                .checked_sub(origin)
                .ok_or_else(|| Box::<str>::from("fragment PTS underflowed the track origin"))?;
            let duration = u32::try_from(pending.duration)
                .map_err(|_| Box::<str>::from("sample duration exceeds the ISOBMFF range"))?;
            samples.push(Sample::new(
                pending.data,
                Some(dts),
                Some(pts),
                Some(duration),
                pending.random_access,
            ));
        }
        let first_dts = samples[0].dts.unwrap_or(0);
        let tfdt = u64::try_from(first_dts)
            .map_err(|_| Box::<str>::from("fragment decode time is negative after rebasing"))?;
        let fragment = FragmentTrackData::new(TRACK_ID, tfdt, &samples);
        let bytes =
            build_media_segment(self.sequence, std::slice::from_ref(&fragment)).map_err(mux)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| Box::<str>::from("CMAF fragment sequence number overflowed"))?;
        Ok(Payload::from_bytes(bytes))
    }
}

fn codec_config(track: &DiscoveredTrack) -> Result<CodecConfig, Box<str>> {
    require_extradata(&track.codec_extradata)?;
    let extra = track.codec_extradata.as_bytes();
    match (track.codec, &track.parameters) {
        (Codec::H264, MediaParameters::Video { width, height, .. }) => {
            let record =
                AVCDecoderConfigurationRecord::parse(record_body(extra, *b"avcC")).map_err(mux)?;
            Ok(CodecConfig::Avc {
                config: transmux::AVCConfigurationBox::new(record),
                width: u16_dim(*width, "video width")?,
                height: u16_dim(*height, "video height")?,
            })
        }
        (Codec::Hevc, MediaParameters::Video { width, height, .. }) => {
            let record =
                HEVCDecoderConfigurationRecord::parse(record_body(extra, *b"hvcC")).map_err(mux)?;
            Ok(CodecConfig::Hevc {
                config: transmux::HEVCConfigurationBox::new(record),
                width: u16_dim(*width, "video width")?,
                height: u16_dim(*height, "video height")?,
            })
        }
        (Codec::Av1, MediaParameters::Video { width, height, .. }) => Ok(CodecConfig::Av1 {
            config: Av1ConfigurationBox::parse(record_body(extra, *b"av1C")).map_err(mux)?,
            width: u16_dim(*width, "video width")?,
            height: u16_dim(*height, "video height")?,
        }),
        (
            Codec::Aac,
            MediaParameters::Audio {
                sample_rate,
                channels,
                bit_depth,
                ..
            },
        ) => {
            let mut config = aac_config_from_asc_bytes(extra.to_vec()).map_err(mux)?;
            if let CodecConfig::Aac {
                channel_count,
                sample_rate: rate,
                sample_size,
                ..
            } = &mut config
            {
                *channel_count = channels.get();
                *rate = sample_rate.get();
                if let Some(depth) = *bit_depth {
                    *sample_size = depth.get();
                }
            }
            Ok(config)
        }
        (
            Codec::Opus,
            MediaParameters::Audio {
                sample_rate,
                channels,
                bit_depth,
                timing,
                ..
            },
        ) => Ok(CodecConfig::Opus {
            config: opus_config(
                extra,
                channels.get(),
                sample_rate.get(),
                timing.initial_padding_samples,
            )?,
            channel_count: channels.get(),
            sample_rate: sample_rate.get(),
            sample_size: bit_depth.map_or(16, std::num::NonZeroU16::get),
        }),
        (Codec::MovText | Codec::SubRip | Codec::Text | Codec::WebVtt, _) => {
            Err("subtitle codecs are not supported by the CMAF muxer".into())
        }
        (Codec::Unknown(id), _) => {
            Err(format!("codec {id} is not supported by the CMAF muxer").into())
        }
        _ => Err(format!(
            "{:?} codec parameters do not match {}",
            track.codec, track.id
        )
        .into()),
    }
}

fn opus_config(
    extra: &[u8],
    channels: u16,
    sample_rate: u32,
    initial_padding: u32,
) -> Result<OpusSpecificBox, Box<str>> {
    let body = record_body(extra, *b"dOps");
    if let Ok(parsed) = OpusSpecificBox::parse(body) {
        return Ok(parsed);
    }
    if extra.starts_with(b"OpusHead") {
        return opus_head(extra);
    }
    Ok(OpusSpecificBox {
        version: 0,
        output_channel_count: u8::try_from(channels)
            .map_err(|_| Box::<str>::from("Opus channel count exceeds dOps range"))?,
        pre_skip: u16::try_from(initial_padding.min(u32::from(u16::MAX))).unwrap_or(u16::MAX),
        input_sample_rate: sample_rate,
        output_gain: 0,
        channel_mapping_family: 0,
        channel_mapping: None,
    })
}

fn opus_head(bytes: &[u8]) -> Result<OpusSpecificBox, Box<str>> {
    let body = bytes
        .get(8..)
        .ok_or_else(|| Box::<str>::from("OpusHead is truncated"))?;
    if body.len() < 11 {
        return Err("OpusHead is truncated".into());
    }
    Ok(OpusSpecificBox {
        version: body[0],
        output_channel_count: body[1],
        pre_skip: u16::from_le_bytes([body[2], body[3]]),
        input_sample_rate: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        output_gain: i16::from_le_bytes([body[8], body[9]]),
        channel_mapping_family: body[10],
        channel_mapping: None,
    })
}

fn edit_list(
    padding_ticks: u64,
    first_decode: TickTimestamp,
    first_pts: TickTimestamp,
) -> Option<EditListBox> {
    let entries = if padding_ticks > 0 {
        vec![EditListEntry {
            segment_duration: 0,
            media_time: i64::try_from(padding_ticks).ok()?,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }]
    } else if first_decode > 0 {
        vec![
            EditListEntry {
                segment_duration: u64::try_from(first_decode).ok()?,
                media_time: -1,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            },
            EditListEntry {
                segment_duration: 0,
                media_time: 0,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            },
        ]
    } else {
        let composition = first_pts.saturating_sub(first_decode);
        if composition <= 0 {
            return None;
        }
        vec![EditListEntry {
            segment_duration: 0,
            media_time: composition,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }]
    };
    let version = u8::from(entries.iter().any(|entry| {
        entry.segment_duration > u64::from(u32::MAX)
            || entry.media_time < i64::from(i32::MIN)
            || entry.media_time > i64::from(i32::MAX)
    }));
    Some(EditListBox {
        version,
        flags: 0,
        entries,
    })
}

fn with_cmaf_init(init: &[u8], elst: Option<EditListBox>) -> Result<Vec<u8>, Box<str>> {
    let (_, moov_bytes) = split_ftyp_moov(init)?;
    let ftyp = FileTypeBox {
        major_brand: *b"iso6",
        minor_version: 0,
        compatible_brands: vec![*b"iso6", *b"cmfc", *b"iso5", *b"mp41"],
    };
    let mut moov = MovieBox::parse(&moov_bytes).map_err(mux)?;
    if let Some(elst) = elst {
        let track = moov
            .tracks
            .first_mut()
            .ok_or_else(|| Box::<str>::from("initialization segment has no track"))?;
        track.edts = Some(EditBox {
            elst: Some(elst),
            opaque: Vec::new(),
        });
    }
    let mut out = Vec::with_capacity(ftyp.serialized_len() + moov.serialized_len());
    out.extend_from_slice(&ftyp.to_bytes());
    out.extend_from_slice(&moov.to_bytes());
    Ok(out)
}

fn split_ftyp_moov(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Box<str>> {
    let mut offset = 0;
    let mut ftyp = None;
    let mut moov = None;
    while offset + 8 <= bytes.len() {
        let size = usize::try_from(u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("box size is four bytes"),
        ))
        .map_err(|_| Box::<str>::from("ISOBMFF box size exceeds address space"))?;
        if size < 8 || offset + size > bytes.len() {
            return Err("initialization segment is truncated".into());
        }
        let kind = &bytes[offset + 4..offset + 8];
        let box_bytes = bytes[offset..offset + size].to_vec();
        match kind {
            b"ftyp" => ftyp = Some(box_bytes),
            b"moov" => moov = Some(box_bytes),
            _ => {}
        }
        offset += size;
    }
    Ok((
        ftyp.ok_or_else(|| Box::<str>::from("initialization segment has no ftyp"))?,
        moov.ok_or_else(|| Box::<str>::from("initialization segment has no moov"))?,
    ))
}

fn media_timescale(timebase: Timebase) -> Result<u32, Box<str>> {
    let num = timebase.num().get();
    let den = timebase.den().get();
    if !den.is_multiple_of(num) {
        return Err(format!("timebase {num}/{den} is not an integer media timescale").into());
    }
    Ok(den / num)
}

fn padding_ticks(track: &DiscoveredTrack) -> Result<u64, Box<str>> {
    let MediaParameters::Audio {
        sample_rate,
        timing,
        ..
    } = track.parameters
    else {
        return Ok(0);
    };
    samples_to_ticks(
        timing.initial_padding_samples,
        sample_rate.get(),
        track.timebase,
    )
}

fn samples_to_ticks(samples: u32, sample_rate: u32, timebase: Timebase) -> Result<u64, Box<str>> {
    if samples == 0 {
        return Ok(0);
    }
    let numerator = u128::from(samples) * u128::from(timebase.den().get());
    let denominator = u128::from(sample_rate) * u128::from(timebase.num().get());
    if !numerator.is_multiple_of(denominator) {
        return Err("audio priming cannot be represented exactly in the track timebase".into());
    }
    u64::try_from(numerator / denominator)
        .map_err(|_| Box::<str>::from("audio priming overflowed the media timescale"))
}

fn record_body(bytes: &[u8], fourcc: [u8; 4]) -> &[u8] {
    if bytes.len() >= 8 && bytes[4..8] == fourcc {
        &bytes[8..]
    } else {
        bytes
    }
}

fn require_extradata(extradata: &Payload) -> Result<(), Box<str>> {
    if extradata.is_empty() {
        return Err("CMAF pass-through requires codec extradata".into());
    }
    Ok(())
}

fn u16_dim(value: NonZeroU32, field: &str) -> Result<u16, Box<str>> {
    u16::try_from(value.get())
        .map_err(|_| format!("{field} exceeds the ISOBMFF sample-entry range").into())
}

fn sample_payload(sample: &NormalizedSample) -> &Payload {
    match sample {
        NormalizedSample::Video(sample) => &sample.payload,
        NormalizedSample::Audio(sample) => &sample.payload,
        NormalizedSample::Subtitle(sample) => &sample.payload,
    }
}

fn mux(error: impl std::fmt::Display) -> Box<str> {
    error.to_string().into_boxed_str()
}
