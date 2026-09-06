use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use parking_lot::{Condvar, Mutex};
use tokio::sync::{mpsc, oneshot};
use transmux::{DemuxEvent, StreamingTsDemux, TrackSpec};

use crate::{
    domain::{DiscoveredTrack, TrackCatalog, TrackId},
    source::{
        ByteInput, ByteInputError, DiscoveryLimits, DiscoveryProblem, DiscoveryReport, InputLimits,
        InputState, Packet, SourceError,
    },
};

use super::{MpegTsConfig, control::Control, map};

/// Matroska/WebM EBML header. SRT used to probe any container; MPEG-TS is
/// required now, and this fails the obvious leftover before the probe budget.
const EBML_MAGIC: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];

pub enum WorkerEvent {
    Packet(QueuedPacket),
    End(InputState),
    Error(SourceError),
}

pub struct QueuedPacket {
    packet: Packet,
    _permit: PayloadPermit,
}

impl QueuedPacket {
    pub fn payload_len(&self) -> usize {
        self.packet.retained_payload_bytes()
    }

    pub fn into_packet(self) -> Packet {
        self.packet
    }
}

struct PayloadBudget {
    maximum: usize,
    used: Mutex<usize>,
    released: Condvar,
}

impl PayloadBudget {
    fn new(maximum: usize) -> Arc<Self> {
        Arc::new(Self {
            maximum,
            used: Mutex::new(0),
            released: Condvar::new(),
        })
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> PayloadPermit {
        let mut used = self.used.lock();
        while used.saturating_add(bytes) > self.maximum {
            self.released.wait(&mut used);
        }
        *used += bytes;
        PayloadPermit {
            budget: Arc::clone(self),
            bytes,
        }
    }
}

struct PayloadPermit {
    budget: Arc<PayloadBudget>,
    bytes: usize,
}

impl Drop for PayloadPermit {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock();
        *used -= self.bytes;
        self.budget.released.notify_one();
    }
}

pub fn spawn(
    input: Box<dyn ByteInput>,
    config: MpegTsConfig,
    input_limits: InputLimits,
    discovery_limits: DiscoveryLimits,
    control: Arc<Control>,
    discovery: oneshot::Sender<Result<DiscoveryReport, SourceError>>,
    output: mpsc::Sender<WorkerEvent>,
) -> Result<(), SourceError> {
    std::thread::Builder::new()
        .name("rushls-mpegts".into())
        .spawn(move || {
            run(
                input,
                config,
                input_limits,
                discovery_limits,
                &control,
                discovery,
                &output,
            );
        })
        .map(|_| ())
        .map_err(|error| {
            SourceError::Open(format!("could not start MPEG-TS worker: {error}").into())
        })
}

fn run(
    mut input: Box<dyn ByteInput>,
    config: MpegTsConfig,
    input_limits: InputLimits,
    discovery_limits: DiscoveryLimits,
    control: &Control,
    discovery: oneshot::Sender<Result<DiscoveryReport, SourceError>>,
    output: &mpsc::Sender<WorkerEvent>,
) {
    let mut demux = StreamingTsDemux::new();
    let mut builder = CatalogBuilder::new();
    let mut buffer = vec![0_u8; config.read_buffer_size.get()];
    let mut seen_prefix = Vec::new();
    let mut io = WorkerIo {
        input: input.as_mut(),
        demux: &mut demux,
        builder: &mut builder,
        buffer: &mut buffer,
        seen_prefix: &mut seen_prefix,
        control,
    };
    match discover_tracks(&mut io, discovery_limits, input_limits) {
        Ok((catalog, terminal)) => {
            if discovery.send(Ok(catalog)).is_err() {
                return;
            }
            pump(
                &mut io,
                input_limits,
                config.maximum_queued_payload_bytes.get(),
                terminal,
                output,
            );
        }
        Err(error) => {
            let _ = discovery.send(Err(error));
        }
    }
}

struct WorkerIo<'a> {
    input: &'a mut dyn ByteInput,
    demux: &'a mut StreamingTsDemux,
    builder: &'a mut CatalogBuilder,
    buffer: &'a mut [u8],
    seen_prefix: &'a mut Vec<u8>,
    control: &'a Control,
}

fn discover_tracks(
    io: &mut WorkerIo<'_>,
    discovery_limits: DiscoveryLimits,
    input_limits: InputLimits,
) -> Result<(DiscoveryReport, Option<InputState>), SourceError> {
    if discovery_limits.maximum_probe_bytes == 0 || discovery_limits.maximum_wall_time.is_zero() {
        return Err(DiscoveryProblem::LimitNotPositive {
            field: if discovery_limits.maximum_probe_bytes == 0 {
                "maximum probe bytes"
            } else {
                "maximum wall time"
            },
        }
        .into());
    }

    io.control.begin_probe(discovery_limits.maximum_probe_bytes);
    io.control
        .set_deadline(Some(Instant::now() + discovery_limits.maximum_wall_time));

    loop {
        match read_chunk(io.input, io.buffer, io.control, io.seen_prefix) {
            ReadOutcome::Bytes(bytes) => {
                io.demux.feed(bytes);
                io.builder.drain(io.demux, input_limits, false)?;
                if let Some(catalog) = io.builder.take_catalog() {
                    io.control.finish_probe();
                    io.control.set_deadline(None);
                    return Ok((catalog, None));
                }
            }
            ReadOutcome::End(state) => {
                io.demux.finish();
                io.builder.drain(io.demux, input_limits, true)?;
                return io.builder.take_catalog().map_or_else(
                    || Err(discovery_end_error(io.control, io.builder.saw_mpeg_ts())),
                    |catalog| {
                        io.control.finish_probe();
                        io.control.set_deadline(None);
                        Ok((catalog, Some(state)))
                    },
                );
            }
            ReadOutcome::Failed(error) => return Err(error),
            ReadOutcome::Stopped => return Err(discovery_stop_error(io.control)),
        }
    }
}

fn pump(
    io: &mut WorkerIo<'_>,
    input_limits: InputLimits,
    queued_payload_bytes: usize,
    terminal: Option<InputState>,
    output: &mpsc::Sender<WorkerEvent>,
) {
    let budget = PayloadBudget::new(queued_payload_bytes);
    if !emit_prefetch(io.builder, &budget, output) {
        return;
    }
    if let Some(state) = terminal {
        let _ = output.blocking_send(WorkerEvent::End(state));
        return;
    }

    loop {
        match read_chunk(io.input, io.buffer, io.control, io.seen_prefix) {
            ReadOutcome::Bytes(bytes) => {
                io.demux.feed(bytes);
                if let Err(error) = emit_live(io.builder, io.demux, input_limits, &budget, output) {
                    let _ = output.blocking_send(WorkerEvent::Error(error));
                    return;
                }
            }
            ReadOutcome::End(state) => {
                io.demux.finish();
                if let Err(error) = emit_live(io.builder, io.demux, input_limits, &budget, output) {
                    let _ = output.blocking_send(WorkerEvent::Error(error));
                    return;
                }
                let _ = output.blocking_send(WorkerEvent::End(state));
                return;
            }
            ReadOutcome::Failed(error) => {
                let _ = output.blocking_send(WorkerEvent::Error(error));
                return;
            }
            ReadOutcome::Stopped => return,
        }
    }
}

enum ReadOutcome<'a> {
    Bytes(&'a [u8]),
    End(InputState),
    Failed(SourceError),
    Stopped,
}

fn read_chunk<'a>(
    input: &mut dyn ByteInput,
    buffer: &'a mut [u8],
    control: &Control,
    seen_prefix: &mut Vec<u8>,
) -> ReadOutcome<'a> {
    if control.cancelled() {
        return ReadOutcome::Stopped;
    }
    let requested = control.limit_read(buffer.len());
    if requested == 0 {
        return ReadOutcome::Failed(discovery_stop_error(control));
    }
    match input.read(&mut buffer[..requested], control) {
        Ok(0) => ReadOutcome::End(InputState::Interrupted),
        Ok(read) => {
            control.record_read(read);
            let bytes = &buffer[..read];
            if let Err(error) = observe_prefix(seen_prefix, bytes) {
                return ReadOutcome::Failed(error);
            }
            ReadOutcome::Bytes(bytes)
        }
        Err(ByteInputError::End(state)) => ReadOutcome::End(state),
        Err(ByteInputError::Failed(error)) => ReadOutcome::Failed(SourceError::Input(error)),
    }
}

fn observe_prefix(seen: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SourceError> {
    if seen.len() >= EBML_MAGIC.len() {
        return Ok(());
    }
    let needed = EBML_MAGIC.len() - seen.len();
    seen.extend_from_slice(&bytes[..bytes.len().min(needed)]);
    if seen.len() >= EBML_MAGIC.len() && seen.starts_with(&EBML_MAGIC) {
        return Err(SourceError::Demux("SRT ingest requires MPEG-TS".into()));
    }
    Ok(())
}

fn discovery_stop_error(control: &Control) -> SourceError {
    if control.cancelled() {
        DiscoveryProblem::Abandoned.into()
    } else if control.probe_exceeded() {
        DiscoveryProblem::ProbeLimitExceeded.into()
    } else if control.deadline_exceeded() {
        DiscoveryProblem::DeadlineExceeded.into()
    } else {
        DiscoveryProblem::Abandoned.into()
    }
}

fn discovery_end_error(control: &Control, saw_mpeg_ts: bool) -> SourceError {
    if control.probe_exceeded() {
        return DiscoveryProblem::ProbeLimitExceeded.into();
    }
    if control.deadline_exceeded() {
        return DiscoveryProblem::DeadlineExceeded.into();
    }
    if saw_mpeg_ts {
        DiscoveryProblem::Abandoned.into()
    } else {
        SourceError::Demux("SRT ingest requires MPEG-TS".into())
    }
}

struct CatalogBuilder {
    pending: BTreeMap<u32, DiscoveredTrack>,
    av1: BTreeMap<u32, (TrackSpec, Option<bool>)>,
    skipped: BTreeSet<u32>,
    order: Vec<u32>,
    prefetch: VecDeque<Packet>,
    catalog: Option<DiscoveryReport>,
    saw_mpeg_ts: bool,
    /// `TracksResolved` froze the kind set; first PTS still has to land.
    resolved: bool,
}

impl CatalogBuilder {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            av1: BTreeMap::new(),
            skipped: BTreeSet::new(),
            order: Vec::new(),
            prefetch: VecDeque::new(),
            catalog: None,
            saw_mpeg_ts: false,
            resolved: false,
        }
    }

    fn saw_mpeg_ts(&self) -> bool {
        self.saw_mpeg_ts
    }

    fn take_catalog(&mut self) -> Option<DiscoveryReport> {
        self.catalog.clone()
    }

    fn drain(
        &mut self,
        demux: &mut StreamingTsDemux,
        limits: InputLimits,
        finishing: bool,
    ) -> Result<(), SourceError> {
        while let Some(event) = demux.poll_event() {
            self.saw_mpeg_ts = true;
            match event {
                DemuxEvent::TrackAdded(spec) => self.add_track(&spec)?,
                DemuxEvent::TrackUpdated(spec) => self.update_track(&spec)?,
                DemuxEvent::TrackRemoved { track_id, .. }
                | DemuxEvent::TrackAbandoned {
                    track_id: Some(track_id),
                    ..
                } => {
                    self.remove_track(track_id)?;
                }
                DemuxEvent::Sample {
                    track_id, sample, ..
                } => {
                    self.push_sample(track_id, sample, limits)?;
                }
                DemuxEvent::TracksResolved { .. } => {
                    self.resolved = true;
                    self.try_freeze(false)?;
                }
                DemuxEvent::Discontinuity { track, .. } if self.catalog.is_some() => {
                    return Err(SourceError::Demux(
                        match track {
                            Some(track) => format!("MPEG-TS discontinuity on track {track}"),
                            None => "MPEG-TS discontinuity".into(),
                        }
                        .into(),
                    ));
                }
                _ => {}
            }
        }
        if finishing && self.catalog.is_none() && self.saw_mpeg_ts {
            self.try_freeze(true)?;
        }
        Ok(())
    }

    fn remove_track(&mut self, track_id: u32) -> Result<(), SourceError> {
        if self.catalog.is_some() && self.pending.contains_key(&track_id) {
            return Err(SourceError::TrackSetChanged);
        }
        self.av1.remove(&track_id);
        self.pending.remove(&track_id);
        self.order.retain(|id| *id != track_id);
        self.skipped.insert(track_id);
        Ok(())
    }

    fn add_track(&mut self, spec: &TrackSpec) -> Result<(), SourceError> {
        if super::av1::configuration(spec)?.is_some() {
            if self.catalog.is_some() && !self.av1.contains_key(&spec.track_id) {
                return Err(SourceError::TrackSetChanged);
            }
            self.av1
                .entry(spec.track_id)
                .or_insert_with(|| (spec.clone(), None));
            return Ok(());
        }

        if self.catalog.is_some() {
            if map::track(spec)?.is_some() && !self.pending.contains_key(&spec.track_id) {
                return Err(SourceError::TrackSetChanged);
            }
            return Ok(());
        }
        match map::track(spec)? {
            Some(track) => {
                self.order.push(spec.track_id);
                self.pending.insert(spec.track_id, track);
            }
            None => {
                self.skipped.insert(spec.track_id);
            }
        }
        Ok(())
    }

    fn update_track(&mut self, spec: &TrackSpec) -> Result<(), SourceError> {
        if let Some((existing, _)) = self.av1.get(&spec.track_id) {
            if super::av1::configuration(existing)?
                .map(|config| broadcast_common::Serialize::to_bytes(&config))
                != super::av1::configuration(spec)?
                    .map(|config| broadcast_common::Serialize::to_bytes(&config))
            {
                return Err(SourceError::CodecParametersChanged {
                    track_id: TrackId(spec.track_id),
                });
            }
            return Ok(());
        }
        let Some(mut mapped) = map::track(spec)? else {
            if self.pending.contains_key(&spec.track_id) {
                return Err(SourceError::TrackSetChanged);
            }
            self.skipped.insert(spec.track_id);
            return Ok(());
        };
        match self.pending.get_mut(&spec.track_id) {
            Some(existing) => {
                // The Opus PMT has no pre-skip. Discovery learns it from the
                // first PES control header, so repeated PMTs cannot reset it.
                if existing.codec == crate::domain::Codec::Opus
                    && let crate::domain::MediaParameters::Audio { timing, .. } =
                        &existing.parameters
                    && let crate::domain::MediaParameters::Audio {
                        timing: updated, ..
                    } = &mut mapped.parameters
                {
                    updated.initial_padding_samples = timing.initial_padding_samples;
                }
                if existing.codec != mapped.codec
                    || existing.parameters != mapped.parameters
                    || existing.codec_extradata != mapped.codec_extradata
                {
                    return Err(SourceError::CodecParametersChanged {
                        track_id: mapped.id,
                    });
                }
            }
            None if self.catalog.is_some() => return Err(SourceError::TrackSetChanged),
            None => {
                self.order.push(spec.track_id);
                self.pending.insert(spec.track_id, mapped);
            }
        }
        Ok(())
    }

    fn push_sample(
        &mut self,
        track_id: u32,
        mut sample: transmux::Sample,
        limits: InputLimits,
    ) -> Result<(), SourceError> {
        if let Some((spec, reduced)) = self.av1.get_mut(&track_id) {
            if let Some((track, is_reduced)) = super::av1::track(spec, &sample.data)? {
                if let Some(existing) = self.pending.get(&track_id) {
                    if existing.codec_extradata != track.codec_extradata {
                        return Err(SourceError::CodecParametersChanged { track_id: track.id });
                    }
                } else {
                    self.order.push(track_id);
                    self.pending.insert(track_id, track);
                }
                *reduced = Some(is_reduced);
            }
            let Some(reduced) = *reduced else {
                return Ok(());
            };
            sample.flags.is_sync =
                crate::media::av1::keyframe(&sample.data, reduced).ok_or_else(|| {
                    SourceError::Demux("AV1G PES has no complete frame header".into())
                })?;
        }
        if self.skipped.contains(&track_id) {
            return Ok(());
        }
        let Some(track) = self.pending.get_mut(&track_id) else {
            return Ok(());
        };
        if track.codec == crate::domain::Codec::Opus {
            self.prefetch.extend(super::opus::packets(
                track,
                &sample,
                limits.maximum_payload_bytes_per_packet,
            )?);
            return self.try_freeze(false);
        }
        let packet = map::packet(
            TrackId(track_id),
            sample,
            limits.maximum_payload_bytes_per_packet,
        )?;
        crate::source::record_first_pts(track, &packet)?;
        self.prefetch.push_back(packet);
        self.try_freeze(false)
    }

    fn tracks_have_timestamps(&self) -> bool {
        self.av1.values().all(|(_, reduced)| reduced.is_some())
            && !self.pending.is_empty()
            && self.pending.values().all(|track| track.first_pts.is_some())
    }

    fn try_freeze(&mut self, finishing: bool) -> Result<(), SourceError> {
        if self.catalog.is_some() {
            return Ok(());
        }
        if self.pending.is_empty() {
            if finishing {
                return Err(SourceError::Demux(
                    "MPEG-TS contained no H.264, HEVC, AV1, AAC, or Opus track".into(),
                ));
            }
            return Ok(());
        }
        // Calibration reads first_pts off the catalog. Codec configs freeze
        // the kind set; coded frames freeze the timestamps.
        if !self.tracks_have_timestamps() {
            if finishing {
                return Err(SourceError::Demux(
                    "MPEG-TS tracks resolved without a first timestamp".into(),
                ));
            }
            return Ok(());
        }
        if !self.resolved && !finishing {
            return Ok(());
        }
        let tracks: Vec<DiscoveredTrack> = self
            .order
            .iter()
            .filter_map(|id| self.pending.get(id).cloned())
            .collect();
        self.catalog = Some(DiscoveryReport {
            tracks: TrackCatalog::new(tracks)?,
        });
        Ok(())
    }
}

fn emit_prefetch(
    builder: &mut CatalogBuilder,
    budget: &Arc<PayloadBudget>,
    output: &mpsc::Sender<WorkerEvent>,
) -> bool {
    while let Some(packet) = builder.prefetch.pop_front() {
        if !send_packet(packet, budget, output) {
            return false;
        }
    }
    true
}

fn emit_live(
    builder: &mut CatalogBuilder,
    demux: &mut StreamingTsDemux,
    limits: InputLimits,
    budget: &Arc<PayloadBudget>,
    output: &mpsc::Sender<WorkerEvent>,
) -> Result<(), SourceError> {
    builder.drain(demux, limits, false)?;
    while let Some(packet) = builder.prefetch.pop_front() {
        if !send_packet(packet, budget, output) {
            return Ok(());
        }
    }
    Ok(())
}

fn send_packet(
    packet: Packet,
    budget: &Arc<PayloadBudget>,
    output: &mpsc::Sender<WorkerEvent>,
) -> bool {
    let permit = budget.reserve(packet.retained_payload_bytes());
    output
        .blocking_send(WorkerEvent::Packet(QueuedPacket {
            packet,
            _permit: permit,
        }))
        .is_ok()
}
