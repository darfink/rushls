//! CMAF initialization and media fragments from transmux box builders.
//!
//! The live cut loop stays in [`super::CmafTrack`]. This module only writes
//! `ftyp`/`moov` and `styp`/`moof`/`mdat`, plus the edit list that maps encoder
//! delay and a delayed first decode time onto the presentation timeline.
//!
//! Flushing never asks the pipeline budget for memory. Each sample's output
//! bytes (payload plus a fixed box allowance) are reserved by [`CmafOutput::reserve`]
//! when the muxer accepts the sample, before any coordinator or partition
//! queue holds it. That charge is handed to `write`, and a flush serializes
//! into one exactly sized buffer carved out of the accumulated charges. A full
//! pipeline can therefore always drain what it accepted; exhaustion surfaces
//! on acceptance, attributed to a sample.

use std::num::NonZeroU32;

use broadcast_common::{Parse, Serialize};
use bytes::Bytes;
use transmux::{
    AVCDecoderConfigurationRecord, Av1ConfigurationBox, CodecConfig, EditBox, EditListBox,
    EditListEntry, FileTypeBox, HEVCDecoderConfigurationRecord, MovieBox, MovieFragmentBox,
    MovieFragmentHeaderBox, SampleToGroupBox, SegmentTypeBox, TrackFragmentBaseMediaDecodeTimeBox,
    TrackFragmentBox, TrackFragmentHeaderBox, TrackFragmentRunBox, TrackSpec, TrunSample,
    aac_config_from_asc_bytes, build_init_segment,
    movie_fragment::{
        TFHD_DEFAULT_BASE_IS_MOOF, TRUN_DATA_OFFSET_PRESENT,
        TRUN_SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT, TRUN_SAMPLE_DURATION_PRESENT,
        TRUN_SAMPLE_FLAGS_PRESENT, TRUN_SAMPLE_SIZE_PRESENT,
    },
};

use crate::{
    domain::{
        BudgetExceeded, Codec, DiscoveredTrack, MediaParameters, Payload, PipelineBudget,
        Reservation, Stage, TickDuration, TickTimestamp, Timebase,
    },
    media::NormalizedMedia,
};

/// ISOBMFF track id for a one-rendition CMAF output. The number is arbitrary
/// as long as init and fragments agree; 1 is the conventional first track.
const TRACK_ID: u32 = 1;
/// ISO/IEC 14496-12 sample flags, matching transmux's fragment builder.
const SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;
const SAMPLE_FLAGS_NON_SYNC: u32 = 0x0101_0000;
/// Box bytes one run adds around its samples: `styp` (24), `moof` with
/// `mfhd`/`traf`/`tfhd`/`tfdt`/`trun` headers (~100), the `mdat` header (8)
/// and an Opus `sbgp` header (20). Rounded up; flushes verify the bound.
///
/// Every sample reserves one: a run starts at each fragment's first sample,
/// and which sample that is depends on cuts made after acceptance. Runs never
/// outnumber samples, so this always suffices, and the slack is released at
/// each flush.
const RUN_OVERHEAD: usize = 256;
/// Per-sample box bytes: a 16-byte `trun` entry and at most one 8-byte `sbgp`
/// entry.
const SAMPLE_OVERHEAD: usize = 32;
/// Initialization segments carry the codec record and a few fixed boxes.
const INIT_OVERHEAD: usize = 64 * 1024;

pub(super) struct CmafOutput {
    budget: PipelineBudget,
    /// Reserved at open, so publishing the init segment needs no new memory.
    init_charge: Option<Reservation>,
    /// Output bytes reserved for everything in `pending`, accumulated from
    /// the charges taken when each sample was accepted.
    pending_charge: Option<Reservation>,
    spec: TrackSpec,
    codec: Codec,
    nal_length_bytes: usize,
    roll: Option<super::roll::RollRecovery>,
    video: crate::media::video_config::VideoProperties,
    pending: Vec<PendingSample>,
    /// The first decode time on the shared presentation clock. Only a start
    /// before the origin (priming, or a reordered picture's earlier DTS)
    /// moves this track's media clock; see [`media_shift`].
    first_decode: Option<TickTimestamp>,
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
    pub(super) fn open(track: &DiscoveredTrack, budget: PipelineBudget) -> Result<Self, Box<str>> {
        let timescale = media_timescale(track.timebase)?;
        let config = codec_config(track)?;
        let init_bound = track
            .codec_extradata
            .len()
            .saturating_mul(8)
            .saturating_add(INIT_OVERHEAD);
        Ok(Self {
            init_charge: Some(
                budget
                    .try_reserve(init_bound, Stage::MuxOutput)
                    .map_err(mux)?,
            ),
            pending_charge: None,
            budget,
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
            pending: Vec::new(),
            first_decode: None,
            initialized: false,
            sequence: 1,
        })
    }

    /// The output charge for one accepted sample. Taken before the sample is
    /// queued anywhere, so writing and flushing it later cannot fail for memory.
    pub(super) fn reserve(&self, payload_len: usize) -> Result<Reservation, BudgetExceeded> {
        self.budget.try_reserve(
            payload_len
                .saturating_add(SAMPLE_OVERHEAD)
                .saturating_add(RUN_OVERHEAD),
            Stage::MuxOutput,
        )
    }

    pub(super) fn write(
        &mut self,
        sample: &NormalizedMedia,
        pts: TickTimestamp,
        dts: TickTimestamp,
        charge: Reservation,
    ) {
        // Opus permits shortening the final sample duration to discard end padding.
        // Keep startup trim in elst so decode timestamps retain the encoder history.
        let duration = match sample {
            NormalizedMedia::Audio(audio) if self.codec == Codec::Opus => sample
                .duration()
                .saturating_sub(u64::from(audio.trim.trailing_samples)),
            _ => sample.duration(),
        };
        self.admit(
            PendingSample {
                dts,
                pts,
                duration,
                random_access: sample.random_access(),
                data: sample_payload(sample).bytes(),
            },
            charge,
        );
        if self.first_decode.is_none() {
            self.first_decode = Some(dts);
        }
        if !self.initialized {
            self.video.observe_hdr(
                self.codec,
                self.nal_length_bytes,
                sample_payload(sample).as_bytes(),
            );
        }
    }

    /// Queue a sample with the output charge taken when it was accepted.
    fn admit(&mut self, sample: PendingSample, charge: Reservation) {
        match &mut self.pending_charge {
            Some(pending) => pending.absorb(charge),
            None => self.pending_charge = Some(charge),
        }
        self.pending.push(sample);
    }

    fn is_video(&self) -> bool {
        matches!(self.codec, Codec::H264 | Codec::Hevc | Codec::Av1)
    }

    pub(super) fn flush_fragment(&mut self) -> Result<Payload, Box<str>> {
        if !self.initialized {
            let payload = self.build_init()?;
            self.initialized = true;
            return Ok(payload);
        }
        self.build_media()
    }

    pub(super) fn gap(&mut self) {
        // Pre-gap packets do not provide contiguous roll history after absence.
        // Keep the initialization/edit list; initial padding is never reapplied.
        if let Some(roll) = &mut self.roll {
            *roll = super::roll::RollRecovery::default();
        }
    }

    pub(super) fn finalize(&mut self) {
        self.pending.clear();
        self.pending_charge = None;
    }

    fn build_init(&mut self) -> Result<Payload, Box<str>> {
        let mut charge = self
            .init_charge
            .take()
            .ok_or("CMAF initialization was already built")?;
        // Builder intermediates here are a few KiB and are not accounted.
        let init = build_init_segment(std::slice::from_ref(&self.spec), self.spec.timescale)
            .map_err(mux)?;
        let elst = edit_list(media_shift(self.first_decode.unwrap_or(0)));
        let bytes = with_cmaf_init(&init, elst, self.roll.is_some(), &self.video)?;
        if bytes.capacity() > charge.bytes() {
            return Err("CMAF initialization exceeded its reserved bound".into());
        }
        let retained = charge.split(bytes.capacity());
        Ok(Payload::reserved(bytes, retained))
    }

    fn build_media(&mut self) -> Result<Payload, Box<str>> {
        if self.pending.is_empty() {
            return Ok(Payload::default());
        }
        // Media time = shared presentation time + shift, so tfdt never goes
        // negative and a track that starts late keeps its later tfdt.
        let origin = -media_shift(self.first_decode.unwrap_or(0));
        let video = self.is_video();
        let styp = SegmentTypeBox {
            major_brand: *b"msdh",
            minor_version: 0,
            compatible_brands: vec![*b"msdh", *b"msix"],
        };
        // Plan every run's boxes first. They are small, and their sizes let
        // the output be allocated once, at its exact length.
        let mut runs = Vec::new();
        let mut sequence = self.sequence;
        let mut start = 0;
        for end in 1..=self.pending.len() {
            if end < self.pending.len() && !(video && self.pending[end].random_access) {
                continue;
            }
            // Each keyframe gets its own moof, so trick play can fetch it
            // without preceding dependent frames. Parts retain their cadence.
            runs.push(plan_run(
                &self.pending[start..end],
                origin,
                sequence,
                styp.serialized_len(),
                self.roll.as_mut(),
            )?);
            sequence = sequence
                .checked_add(1)
                .ok_or("CMAF fragment sequence number overflowed")?;
            start = end;
        }
        let total = runs
            .iter()
            .try_fold(0_usize, |total, run| total.checked_add(run.len))
            .ok_or("CMAF allocation size overflow")?;
        let mut charge = self
            .pending_charge
            .take()
            .ok_or("CMAF samples were queued without an output reservation")?;
        if total > charge.bytes() {
            return Err("CMAF serialization exceeded its reserved bound".into());
        }
        let mut out = Vec::with_capacity(total);
        let mut samples = self.pending.drain(..);
        for run in &runs {
            put(&mut out, &styp)?;
            write_moof(&mut out, &run.moof, run.groups.as_ref())?;
            header(&mut out, run.data_len.saturating_add(8), *b"mdat")?;
            // Each input is dropped once copied, so its lease is released
            // while the rest of the part is still being written.
            for sample in samples.by_ref().take(run.count) {
                out.extend_from_slice(&sample.data);
            }
        }
        drop(samples);
        if out.len() != total {
            return Err("CMAF serialization did not match its planned size".into());
        }
        self.sequence = sequence;
        // The remainder of the per-sample allowance is released here.
        let retained = charge.split(out.capacity().min(charge.bytes()));
        Ok(Payload::reserved(out, retained))
    }
}

/// One `styp`/`moof`/`mdat` run, sized before anything is copied.
struct RunPlan {
    moof: MovieFragmentBox,
    groups: Option<SampleToGroupBox>,
    count: usize,
    data_len: usize,
    len: usize,
}

fn plan_run(
    samples: &[PendingSample],
    origin: TickTimestamp,
    sequence: u32,
    styp_len: usize,
    roll: Option<&mut super::roll::RollRecovery>,
) -> Result<RunPlan, Box<str>> {
    let mut entries = Vec::with_capacity(samples.len());
    let mut any_cts = false;
    let mut data_len = 0_usize;
    let mut first_dts = None;
    for sample in samples {
        let dts = sample
            .dts
            .checked_sub(origin)
            .ok_or("fragment DTS underflowed the track origin")?;
        let pts = sample
            .pts
            .checked_sub(origin)
            .ok_or("fragment PTS underflowed the track origin")?;
        first_dts.get_or_insert(dts);
        let offset = pts
            .checked_sub(dts)
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or("composition offset exceeds the ISOBMFF range")?;
        any_cts |= offset != 0;
        data_len = data_len
            .checked_add(sample.data.len())
            .ok_or("CMAF allocation size overflow")?;
        entries.push(TrunSample {
            sample_duration: Some(
                u32::try_from(sample.duration)
                    .map_err(|_| "sample duration exceeds the ISOBMFF range")?,
            ),
            sample_size: Some(
                u32::try_from(sample.data.len()).map_err(|_| "sample exceeds the ISOBMFF range")?,
            ),
            sample_flags: Some(if sample.random_access {
                SAMPLE_FLAGS_SYNC
            } else {
                SAMPLE_FLAGS_NON_SYNC
            }),
            sample_composition_time_offset: Some(offset),
        });
    }
    let tfdt = first_dts
        .and_then(|dts| u64::try_from(dts).ok())
        .ok_or("fragment decode time is negative after rebasing")?;
    let mut tr_flags = TRUN_DATA_OFFSET_PRESENT
        | TRUN_SAMPLE_DURATION_PRESENT
        | TRUN_SAMPLE_SIZE_PRESENT
        | TRUN_SAMPLE_FLAGS_PRESENT;
    // Version 1 carries a signed composition offset (needed for B-frames).
    let version = if any_cts {
        tr_flags |= TRUN_SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT;
        1
    } else {
        for entry in &mut entries {
            entry.sample_composition_time_offset = None;
        }
        0
    };
    let mut moof = MovieFragmentBox {
        mfhd: MovieFragmentHeaderBox::new(sequence),
        traf: vec![TrackFragmentBox {
            tfhd: TrackFragmentHeaderBox {
                flags: TFHD_DEFAULT_BASE_IS_MOOF,
                track_id: TRACK_ID,
                base_data_offset: None,
                sample_description_index: None,
                default_sample_duration: None,
                default_sample_size: None,
                default_sample_flags: None,
            },
            tfdt: Some(TrackFragmentBaseMediaDecodeTimeBox::new_v1(tfdt)),
            trun: vec![TrackFragmentRunBox {
                version,
                tr_flags,
                data_offset: Some(0),
                first_sample_flags: None,
                samples: entries,
            }],
        }],
    };
    let groups = roll
        .map(|roll| roll.groups(samples.iter().map(|sample| sample.data.as_ref())))
        .transpose()?;
    let moof_len = moof
        .serialized_len()
        .saturating_add(groups.as_ref().map_or(0, Serialize::serialized_len));
    // With default-base-is-moof, sample data starts after moof and the
    // 8-byte mdat header. The offset's size is fixed, so moof_len is final.
    moof.traf[0].trun[0].data_offset = Some(
        i32::try_from(moof_len.saturating_add(8)).map_err(|_| "fragment data offset overflow")?,
    );
    Ok(RunPlan {
        moof,
        groups,
        count: samples.len(),
        data_len,
        len: styp_len
            .saturating_add(moof_len)
            .saturating_add(8)
            .saturating_add(data_len),
    })
}

/// transmux's `traf` model has no sample-group children, so Opus fragments
/// frame `moof { mfhd, traf { tfhd, tfdt, trun, sbgp } }` by hand.
fn write_moof(
    out: &mut Vec<u8>,
    moof: &MovieFragmentBox,
    groups: Option<&SampleToGroupBox>,
) -> Result<(), Box<str>> {
    let Some(groups) = groups else {
        return put(out, moof);
    };
    let [traf] = moof.traf.as_slice() else {
        return Err("Opus rendition must have one fragment track".into());
    };
    let traf_len = traf
        .serialized_len()
        .saturating_add(groups.serialized_len());
    header(
        out,
        traf_len
            .saturating_add(moof.mfhd.serialized_len())
            .saturating_add(8),
        *b"moof",
    )?;
    put(out, &moof.mfhd)?;
    header(out, traf_len, *b"traf")?;
    put(out, &traf.tfhd)?;
    if let Some(tfdt) = &traf.tfdt {
        put(out, tfdt)?;
    }
    for run in &traf.trun {
        put(out, run)?;
    }
    put(out, groups)
}

fn header(out: &mut Vec<u8>, len: usize, kind: [u8; 4]) -> Result<(), Box<str>> {
    let len = u32::try_from(len).map_err(|_| "fragment box size overflow")?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&kind);
    Ok(())
}

/// Serialize in place at the end of `out`, which is already sized exactly.
fn put<T>(out: &mut Vec<u8>, item: &T) -> Result<(), Box<str>>
where
    T: Serialize,
    T::Error: std::fmt::Display,
{
    let start = out.len();
    out.resize(start.saturating_add(item.serialized_len()), 0);
    item.serialize_into(&mut out[start..]).map_err(mux)?;
    Ok(())
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
        (
            Codec::Flac,
            MediaParameters::Audio {
                sample_rate,
                channels,
                bit_depth,
                ..
            },
        ) => {
            let (_, config) = crate::media::flac::configuration(extra)?;
            Ok(CodecConfig::Flac {
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

/// How far this track's media clock runs ahead of the shared presentation
/// clock: the distance its first decode time lies before the shared origin.
///
/// Everything that decodes before the origin is either AAC/Opus priming or
/// a reordered picture's early DTS, and the edit's `media_time` hides exactly
/// that span. A track that starts at or after the origin needs no shift: its
/// late start is carried by `tfdt` alone, because hls.js and Shaka read
/// sample times from `tfdt` and Chrome's MSE ignores empty edits. Its priming,
/// if any, then presents just before its audible start, as Apple's own
/// segmenters do: an edit can only hide media before presentation zero.
fn media_shift(first_decode: TickTimestamp) -> TickTimestamp {
    first_decode.min(0).saturating_neg()
}

/// The single non-empty edit that maps presentation zero to `media_time`.
///
/// Never an empty edit, and never more than one entry: Chrome applies only a
/// first entry with a non-negative `media_time`, and HLS players that skip
/// `elst` entirely see the same timeline whenever the shift is zero.
fn edit_list(media_time: TickTimestamp) -> Option<EditListBox> {
    if media_time == 0 {
        return None;
    }
    let version = u8::from(media_time > i64::from(i32::MAX));
    Some(EditListBox {
        version,
        flags: 0,
        entries: vec![EditListEntry {
            segment_duration: 0,
            media_time,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }],
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

fn sample_payload(sample: &NormalizedMedia) -> &Payload {
    match sample {
        NormalizedMedia::Video(sample) => &sample.payload,
        NormalizedMedia::Audio(sample) => &sample.payload,
        NormalizedMedia::Subtitle(sample) => &sample.payload,
        NormalizedMedia::Gap(_) => unreachable!("gaps have no codec payload"),
    }
}

fn mux(error: impl std::fmt::Display) -> Box<str> {
    error.to_string().into_boxed_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_output(budget: PipelineBudget) -> Result<CmafOutput, Box<dyn std::error::Error>> {
        use crate::{
            domain::{MediaKind, fixtures::TrackBuilder},
            mux::fixtures::H264_EXTRADATA,
        };
        let track = TrackBuilder::new(0, MediaKind::Video)
            .codec_extradata(H264_EXTRADATA.to_vec())
            .build();
        Ok(CmafOutput::open(&track, budget).map_err(|error| error.to_string())?)
    }

    #[test]
    fn keyframes_inside_one_part_have_independent_fragment_ranges()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::mux::fixtures::{H264_IDR, H264_P};
        let budget = PipelineBudget::with_reserve(1024 * 1024, 256 * 1024);
        let mut output = h264_output(budget.clone())?;
        let init_charge = budget.used();
        output.first_decode = Some(0);
        let mut expected = Vec::new();
        for index in 0..6 {
            let data = Bytes::from_static(if index % 2 == 0 { H264_IDR } else { H264_P });
            let charge = output.reserve(data.len())?;
            output.admit(
                PendingSample {
                    dts: index,
                    pts: index + 2,
                    duration: 1,
                    random_access: index % 2 == 0,
                    data,
                },
                charge,
            );
        }
        // Golden output: transmux's own builder, one call per keyframe run.
        for (run, start) in [0_i64, 2, 4].into_iter().enumerate() {
            let samples: Vec<_> = (start..start + 2)
                .map(|index| {
                    transmux::Sample::new(
                        Bytes::from_static(if index % 2 == 0 { H264_IDR } else { H264_P }),
                        Some(index),
                        Some(index + 2),
                        Some(1),
                        index % 2 == 0,
                    )
                })
                .collect();
            let tfdt = u64::try_from(start)?;
            expected.extend(transmux::build_media_segment(
                u32::try_from(run + 1)?,
                &[transmux::FragmentTrackData::new(TRACK_ID, tfdt, &samples)],
            )?);
        }
        let payload = output.build_media().map_err(|error| error.to_string())?;
        assert_eq!(payload.as_bytes(), expected.as_slice());

        let mut remaining = payload.as_bytes();
        let mut times = Vec::new();
        while !remaining.is_empty() {
            let (atom, size) = transmux::parse_box(remaining)?;
            if atom.header.box_type.is(b"moof") {
                let moof = transmux::MovieFragmentBox::parse_body(atom.body)?;
                let track = &moof.traf[0];
                times.push(
                    track
                        .tfdt
                        .as_ref()
                        .ok_or("missing tfdt")?
                        .base_media_decode_time(),
                );
                assert_eq!(track.trun[0].samples.len(), 2);
                assert_eq!(
                    track.trun[0].samples[0].sample_composition_time_offset,
                    Some(2)
                );
            } else if atom.header.box_type.is(b"mdat") {
                assert!(atom.body.starts_with(H264_IDR));
                assert!(atom.body.ends_with(H264_P));
            }
            remaining = &remaining[size..];
        }
        assert_eq!(times, [0, 2, 4]);
        assert_eq!(output.sequence, 4);
        // Only the exact output stays charged; the per-sample slack is gone.
        assert_eq!(budget.used(), init_charge + payload.len());
        let stored = payload.into_retained();
        assert_eq!(budget.used(), init_charge);
        assert!(!stored.is_empty());
        Ok(())
    }

    #[test]
    fn only_a_start_before_the_origin_shifts_the_media_clock() {
        // Priming or a reordered DTS before the origin is hidden by one edit.
        assert_eq!(media_shift(-312), 312);
        let edits = edit_list(media_shift(-312)).expect("priming before the origin");
        assert_eq!(edits.entries.len(), 1);
        assert_eq!(edits.entries[0].segment_duration, 0);
        assert_eq!(edits.entries[0].media_time, 312);
        // A start at or after the origin is a later tfdt, never an edit.
        for first_decode in [0, 900, 48_000] {
            assert_eq!(media_shift(first_decode), 0);
            assert!(edit_list(media_shift(first_decode)).is_none());
        }
    }
}
