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
    Sample, TrackSpec, aac_config_from_asc_bytes, build_init_segment, build_media_segment,
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
    codec: Codec,
    nal_length_bytes: usize,
    roll: Option<super::roll::RollRecovery>,
    video: crate::media::video_config::VideoProperties,
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
            nal_length_bytes: match &config {
                CodecConfig::Avc { config, .. } => {
                    usize::from(config.config.length_size_minus_one + 1)
                }
                CodecConfig::Hevc { config, .. } => {
                    usize::from(config.config.length_size_minus_one + 1)
                }
                _ => 0,
            },
            spec: TrackSpec::new(TRACK_ID, timescale, config),
            codec: track.codec,
            roll: (track.codec == Codec::Opus).then(super::roll::RollRecovery::default),
            video: crate::media::video_config::properties(
                track.codec,
                track.codec_extradata.as_bytes(),
            ),
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
            if let NormalizedSample::Audio(audio) = sample {
                // Normalized audio uses one tick per decoded sample.
                self.padding_ticks = self
                    .padding_ticks
                    .max(u64::from(audio.trim.leading_samples));
            }
        }
        if !self.initialized {
            self.video.observe_hdr(
                self.codec,
                self.nal_length_bytes,
                sample_payload(sample).as_bytes(),
            );
        }
        // Opus permits shortening the final sample duration to discard end padding.
        // Keep startup trim in elst so decode timestamps retain the encoder history.
        let duration = match sample {
            NormalizedSample::Audio(audio) if self.codec == Codec::Opus => sample
                .duration()
                .saturating_sub(u64::from(audio.trim.trailing_samples)),
            _ => sample.duration(),
        };
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
        Ok(Payload::from_bytes(with_cmaf_init(
            &init,
            elst,
            self.roll.is_some(),
            &self.video,
        )?))
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
        let bytes = if let Some(roll) = &mut self.roll {
            roll.fragment(&bytes, &samples)?
        } else {
            bytes
        };
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
        ) => {
            let mut config = crate::media::opus::configuration(extra)?;
            // TS learns priming from PES rather than an OpusHead. Preserve it
            // in dOps too, for decoders that use this informational field.
            config.pre_skip = u16::try_from(timing.initial_padding_samples)
                .map_err(|_| Box::<str>::from("Opus pre-skip exceeds the dOps range"))?;
            Ok(CodecConfig::Opus {
                config,
                channel_count: channels.get(),
                sample_rate: sample_rate.get(),
                sample_size: bit_depth.map_or(16, std::num::NonZeroU16::get),
            })
        }
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

fn edit_list(
    padding_ticks: u64,
    first_decode: TickTimestamp,
    first_pts: TickTimestamp,
) -> Option<EditListBox> {
    // tfdt is rebased by first_decode. Select the first audible/composed
    // sample in that media clock, then retain its offset on the shared clock.
    // Priming and a delayed track start can both be present.
    let audible_start = first_pts.checked_add_unsigned(padding_ticks)?;
    let media_time = audible_start.checked_sub(first_decode)?;
    let mut entries = Vec::new();
    if audible_start > 0 {
        entries.push(EditListEntry {
            segment_duration: u64::try_from(audible_start).ok()?,
            media_time: -1,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        });
    }
    if entries.is_empty() && media_time == 0 {
        return None;
    }
    entries.push(EditListEntry {
        segment_duration: 0,
        media_time,
        media_rate_integer: 1,
        media_rate_fraction: 0,
    });
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

fn with_cmaf_init(
    init: &[u8],
    elst: Option<EditListBox>,
    opus: bool,
    video: &crate::media::video_config::VideoProperties,
) -> Result<Vec<u8>, Box<str>> {
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
    if let Some(table) = moov
        .tracks
        .first_mut()
        .and_then(|track| track.mdia.as_mut())
        .and_then(|media| media.minf.as_mut())
        .and_then(|info| info.stbl.as_mut())
    {
        for child in &mut table.children {
            if let transmux::StblChild::Stsd(description) = child {
                for entry in &mut description.entries {
                    let extra = match entry {
                        transmux::SampleEntryVariant::Avc1(entry) => &mut entry.extra_boxes,
                        transmux::SampleEntryVariant::Hevc1(entry) => &mut entry.extra_boxes,
                        _ => continue,
                    };
                    for (kind, data) in [
                        (*b"mdcv", &video.mastering_display),
                        (*b"clli", &video.content_light),
                    ] {
                        if let Some(data) = data {
                            extra.push(transmux::sample_entries::OpaqueBox {
                                box_type: kind,
                                data: data.clone(),
                            });
                        }
                    }
                    if let Some((width, height)) = video.aspect {
                        let ratio = transmux::PixelAspectRatioBox {
                            h_spacing: u32::from(width),
                            v_spacing: u32::from(height),
                        };
                        extra.push(transmux::sample_entries::OpaqueBox {
                            box_type: *b"pasp",
                            data: ratio.to_bytes(),
                        });
                    }
                    if let Some((primaries, transfer, matrix, full_range)) = video.colour {
                        let colour = transmux::ColourInformationBox {
                            colour_type: *b"nclx",
                            nclx: Some(transmux::NclxColourInfo {
                                colour_primaries: u16::from(primaries),
                                transfer_characteristics: u16::from(transfer),
                                matrix_coefficients: u16::from(matrix),
                                full_range_flag: full_range,
                            }),
                            icc_profile: Vec::new(),
                        };
                        extra.push(transmux::sample_entries::OpaqueBox {
                            box_type: *b"colr",
                            data: colour.to_bytes(),
                        });
                    }
                }
            }
        }
    }
    if opus {
        let table = moov
            .tracks
            .first_mut()
            .and_then(|track| track.mdia.as_mut())
            .and_then(|media| media.minf.as_mut())
            .and_then(|info| info.stbl.as_mut())
            .ok_or("Opus init has no sample table")?;
        super::roll::RollRecovery::init(table);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_audio_keeps_both_priming_and_movie_offset() {
        let edits = edit_list(312, 48_000, 48_000).expect("delayed primed audio");
        assert_eq!(edits.entries.len(), 2);
        assert_eq!(edits.entries[0].segment_duration, 48_312);
        assert_eq!(edits.entries[0].media_time, -1);
        assert_eq!(edits.entries[1].media_time, 312);
        let edits = edit_list(312, -312, -312).expect("priming before origin");
        assert_eq!(edits.entries.len(), 1);
        assert_eq!(edits.entries[0].media_time, 312);
    }

    #[test]
    fn delayed_reordered_video_keeps_composition_offset() {
        let edits = edit_list(0, 900, 1_000).expect("delayed reordered video");
        assert_eq!(edits.entries[0].segment_duration, 1_000);
        assert_eq!(edits.entries[1].media_time, 100);
    }
}
