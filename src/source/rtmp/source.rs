//! Streaming RTMP media behind a bounded `PacketSource`.
//!
//! The session task already parsed tags. This adapter waits for decoder
//! configuration, freezes a catalog, then yields CMAF-ready access units on
//! the RTMP millisecond clock.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use cc_rtmp::{CmafCodec, CmafUnit, EncoderSummary, MediaInterpretation};

use crate::{
    domain::{
        Appender, BoxFuture, DiscoveredTrack, MediaKind, SourceTrackKey, TrackCatalog, TrackId,
    },
    observe::SourceMeters,
    source::{
        DiscoveryLimits, DiscoveryProblem, DiscoveryReport, InputLimits, InputState, Packet,
        PacketSource, SourceError,
    },
};

use super::{
    caption, map,
    queue::{IngressEvent, IngressReader},
};

pub struct RtmpPacketSource {
    ingress: Option<IngressReader>,
    limits: InputLimits,
    meters: Arc<dyn SourceMeters>,
    discovery: Option<DiscoveryReport>,
    tracks: LiveTracks,
    prefetch: VecDeque<Packet>,
    pending: VecDeque<Packet>,
    terminal: Option<InputState>,
}

#[derive(Clone, Default)]
struct LiveTracks {
    by_source: HashMap<SourceTrackKey, TrackId>,
    subtitle: Option<TrackId>,
}

impl RtmpPacketSource {
    pub fn new(
        ingress: IngressReader,
        limits: InputLimits,
        meters: Arc<dyn SourceMeters>,
    ) -> Result<Self, SourceError> {
        if limits.maximum_packets_per_batch == 0 {
            return Err(SourceError::Open(
                "maximum packets per batch must be nonzero".into(),
            ));
        }
        if limits.maximum_payload_bytes_per_packet == 0 {
            return Err(SourceError::Open(
                "maximum packet payload must be nonzero".into(),
            ));
        }
        if limits.maximum_payload_bytes_per_batch < limits.maximum_payload_bytes_per_packet {
            return Err(SourceError::Open(
                "maximum batch payload must fit one maximum-sized packet".into(),
            ));
        }
        Ok(Self {
            ingress: Some(ingress),
            limits,
            meters,
            discovery: None,
            tracks: LiveTracks::default(),
            prefetch: VecDeque::new(),
            pending: VecDeque::new(),
            terminal: None,
        })
    }

    async fn next_event(&mut self, wait: bool) -> Option<IngressEvent> {
        let ingress = self.ingress.as_mut()?;
        if wait {
            Some(ingress.recv().await)
        } else {
            ingress.try_recv()
        }
    }
}

impl PacketSource for RtmpPacketSource {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            if let Some(discovery) = &self.discovery {
                return Ok(discovery.clone());
            }
            if self.ingress.is_none() {
                return Err(DiscoveryProblem::AlreadyStarted.into());
            }
            if limits.maximum_probe_bytes == 0 || limits.maximum_wall_time.is_zero() {
                return Err(DiscoveryProblem::LimitNotPositive {
                    field: if limits.maximum_probe_bytes == 0 {
                        "maximum probe bytes"
                    } else {
                        "maximum wall time"
                    },
                }
                .into());
            }

            let deadline = Instant::now() + limits.maximum_wall_time;
            let mut builder = CatalogBuilder::new();
            let mut probed = 0_usize;

            loop {
                if Instant::now() >= deadline {
                    let (catalog, prefetch) =
                        builder.finish_on_limit(DiscoveryProblem::DeadlineExceeded)?;
                    return Ok(self.store_discovery(catalog, prefetch));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let event = match tokio::time::timeout(remaining, self.next_event(true)).await {
                    Ok(Some(event)) => event,
                    Ok(None) => return Err(DiscoveryProblem::Abandoned.into()),
                    Err(_) => {
                        let (catalog, prefetch) =
                            builder.finish_on_limit(DiscoveryProblem::DeadlineExceeded)?;
                        return Ok(self.store_discovery(catalog, prefetch));
                    }
                };
                probed = probed.saturating_add(event_bytes(&event));
                if probed > limits.maximum_probe_bytes {
                    let (catalog, prefetch) =
                        builder.finish_on_limit(DiscoveryProblem::ProbeLimitExceeded)?;
                    return Ok(self.store_discovery(catalog, prefetch));
                }
                match builder.observe(event, self.limits)? {
                    Observe::Continue => {
                        if builder.can_freeze() {
                            // Captions often sit a few tags behind the first
                            // A/V sample. Drain what is already queued so a
                            // file-like publish keeps them in the catalog;
                            // a live queue that is already empty freezes now
                            // and ignores later script-data, matching captions
                            // that arrive after discovery has already frozen.
                            loop {
                                let Some(queued) = self.next_event(false).await else {
                                    break;
                                };
                                probed = probed.saturating_add(event_bytes(&queued));
                                if probed > limits.maximum_probe_bytes {
                                    let (catalog, prefetch) = builder
                                        .finish_on_limit(DiscoveryProblem::ProbeLimitExceeded)?;
                                    return Ok(self.store_discovery(catalog, prefetch));
                                }
                                match builder.observe(queued, self.limits)? {
                                    Observe::Continue => {}
                                    Observe::End(state, catalog, prefetch) => {
                                        self.terminal = Some(state);
                                        let catalog = catalog.ok_or(DiscoveryProblem::Abandoned)?;
                                        return Ok(self.store_discovery(catalog, prefetch));
                                    }
                                    Observe::Failed(error) => return Err(error),
                                }
                            }
                            // Drain can add another Enhanced track that still
                            // needs a first PTS, or metadata-declared extras
                            // that have not arrived yet.
                            if !builder.can_freeze() {
                                continue;
                            }
                            let catalog = builder.freeze()?;
                            let prefetch = std::mem::take(&mut builder.prefetch);
                            return Ok(self.store_discovery(catalog, prefetch));
                        }
                    }
                    Observe::End(state, catalog, prefetch) => {
                        self.terminal = Some(state);
                        let catalog = catalog.ok_or(DiscoveryProblem::Abandoned)?;
                        return Ok(self.store_discovery(catalog, prefetch));
                    }
                    Observe::Failed(error) => return Err(error),
                }
            }
        })
    }

    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        Box::pin(async move {
            if self.discovery.is_none() {
                return Err(SourceError::Input(
                    "RTMP source must be discovered before reading packets".into(),
                ));
            }

            let mut packets = 0_usize;
            let mut payload_bytes = 0_usize;
            loop {
                let packet = if let Some(packet) = self.pending.pop_front() {
                    packet
                } else if let Some(packet) = self.prefetch.pop_front() {
                    packet
                } else if self.terminal.is_some() {
                    break;
                } else {
                    match self.next_event(packets == 0).await {
                        Some(IngressEvent::End(state)) => {
                            self.terminal = Some(state);
                            break;
                        }
                        Some(IngressEvent::Failed(error)) => {
                            return Err(SourceError::Input(error));
                        }
                        Some(event) => {
                            let mut mapped = live_packets(event, &self.tracks, self.limits)?;
                            let Some(packet) = mapped.pop_front() else {
                                continue;
                            };
                            self.pending.extend(mapped);
                            packet
                        }
                        None => {
                            if packets == 0 {
                                self.terminal = Some(InputState::Interrupted);
                            }
                            break;
                        }
                    }
                };
                let next_bytes = payload_bytes
                    .checked_add(packet.retained_payload_bytes())
                    .ok_or_else(|| {
                        SourceError::Input("batch payload accounting overflowed".into())
                    })?;
                if packets == self.limits.maximum_packets_per_batch
                    || next_bytes > self.limits.maximum_payload_bytes_per_batch
                {
                    self.pending.push_front(packet);
                    break;
                }
                payload_bytes = next_bytes;
                packets += 1;
                out.push(packet);
            }

            self.meters
                .source_progress(payload_bytes as u64, packets as u64, 0);
            Ok(self.terminal.unwrap_or(InputState::Open))
        })
    }
}

impl RtmpPacketSource {
    fn store_discovery(
        &mut self,
        catalog: DiscoveryReport,
        prefetch: VecDeque<Packet>,
    ) -> DiscoveryReport {
        self.tracks = LiveTracks {
            by_source: catalog
                .tracks
                .tracks()
                .iter()
                .filter(|track| track.kind() != MediaKind::Subtitle)
                .filter_map(|track| Some((track.source_key.clone()?, track.id)))
                .collect(),
            subtitle: catalog
                .tracks
                .tracks()
                .iter()
                .find(|track| track.kind() == MediaKind::Subtitle)
                .map(|track| track.id),
        };
        self.prefetch = prefetch;
        self.discovery = Some(catalog.clone());
        catalog
    }
}

enum Observe {
    Continue,
    End(InputState, Option<DiscoveryReport>, VecDeque<Packet>),
    Failed(SourceError),
}

struct CatalogBuilder {
    /// Discovery order is HLS rendition order; the first of each kind is DEFAULT.
    tracks: Vec<DiscoveredTrack>,
    text: Option<DiscoveredTrack>,
    next_id: u32,
    hint: Option<EncoderSummary>,
    expected_audio: BTreeSet<u8>,
    expected_video: BTreeSet<u8>,
    prefetch: VecDeque<Packet>,
    saw_sample: bool,
    catalog: Option<DiscoveryReport>,
}

impl CatalogBuilder {
    fn new() -> Self {
        Self {
            tracks: Vec::new(),
            text: None,
            next_id: 0,
            hint: None,
            expected_audio: BTreeSet::new(),
            expected_video: BTreeSet::new(),
            prefetch: VecDeque::new(),
            saw_sample: false,
            catalog: None,
        }
    }

    fn observe(
        &mut self,
        event: IngressEvent,
        limits: InputLimits,
    ) -> Result<Observe, SourceError> {
        match event {
            IngressEvent::Metadata(metadata) => {
                if let MediaInterpretation::Parsed(parsed) = metadata.interpretation {
                    self.hint = Some(parsed.encoder_summary());
                    self.expected_audio = numbered_track_ids(parsed.audio_tracks.keys());
                    self.expected_video = numbered_track_ids(parsed.video_tracks.keys());
                }
                Ok(Observe::Continue)
            }
            IngressEvent::Audio { timestamp, media } => {
                self.on_units(timestamp, media.cmaf_units().map_err(demux)?, limits)
            }
            IngressEvent::Video { timestamp, media } => {
                self.on_units(timestamp, media.cmaf_units().map_err(demux)?, limits)
            }
            IngressEvent::Script { timestamp, payload } => {
                self.on_script(timestamp, &payload, limits)
            }
            IngressEvent::End(state) => Ok(Observe::End(
                state,
                self.take_catalog(),
                std::mem::take(&mut self.prefetch),
            )),
            IngressEvent::Failed(error) => Ok(Observe::Failed(SourceError::Input(error))),
        }
    }

    fn on_script(
        &mut self,
        timestamp: u32,
        payload: &[u8],
        limits: InputLimits,
    ) -> Result<Observe, SourceError> {
        let Some(text) = caption::cue_text(payload) else {
            return Ok(Observe::Continue);
        };
        if self.text.is_none() {
            if self.catalog.is_some() {
                return Ok(Observe::Continue);
            }
            self.text = Some(map::text_track(TrackId(self.next_id)));
            self.next_id += 1;
        }
        let track = self.text.as_mut().expect("text track was just inserted");
        if track.first_pts.is_none() {
            track.first_pts = Some(i64::from(timestamp));
        }
        self.prefetch.push_back(map::text_packet(
            track.id,
            timestamp,
            text,
            limits.maximum_payload_bytes_per_packet,
        )?);
        Ok(Observe::Continue)
    }

    fn on_units(
        &mut self,
        timestamp: u32,
        units: Vec<CmafUnit>,
        limits: InputLimits,
    ) -> Result<Observe, SourceError> {
        for unit in units {
            self.on_unit(timestamp, unit, limits)?;
        }
        Ok(Observe::Continue)
    }

    fn on_unit(
        &mut self,
        timestamp: u32,
        unit: CmafUnit,
        limits: InputLimits,
    ) -> Result<(), SourceError> {
        match unit {
            CmafUnit::Configuration {
                codec,
                extradata,
                track_id,
            } => self.add_track(codec, extradata, track_id),
            sample @ CmafUnit::Sample {
                codec, track_id, ..
            } => {
                self.saw_sample = true;
                let key = map::source_key(codec, track_id);
                let track = self
                    .tracks
                    .iter_mut()
                    .find(|track| track.source_key.as_ref() == Some(&key))
                    .ok_or_else(|| {
                        SourceError::Demux(
                            "RTMP coded frames arrived before a sequence header".into(),
                        )
                    })?;
                if track.first_pts.is_none() {
                    track.first_pts = Some(i64::from(timestamp));
                }
                let packet = map::packet(
                    track.id,
                    timestamp,
                    sample,
                    limits.maximum_payload_bytes_per_packet,
                )?;
                self.prefetch.push_back(packet);
                Ok(())
            }
        }
    }

    fn add_track(
        &mut self,
        codec: CmafCodec,
        extradata: Bytes,
        track_id: Option<u8>,
    ) -> Result<(), SourceError> {
        let mapped = map::track(
            TrackId(self.next_id),
            codec,
            extradata,
            track_id,
            self.hint.as_ref(),
        )?;
        match self
            .tracks
            .iter()
            .find(|track| track.source_key == mapped.source_key)
        {
            Some(existing) => {
                if existing.codec != mapped.codec
                    || existing.parameters != mapped.parameters
                    || existing.codec_extradata != mapped.codec_extradata
                {
                    return Err(SourceError::CodecParametersChanged {
                        track_id: existing.id,
                    });
                }
            }
            None if self.catalog.is_some() => return Err(SourceError::TrackSetChanged),
            None => {
                self.next_id += 1;
                self.tracks.push(mapped);
            }
        }
        Ok(())
    }

    fn can_freeze(&self) -> bool {
        if self.tracks.is_empty() {
            return false;
        }
        // Timeline calibration needs a first PTS on every A/V track. Configs
        // alone freeze the kind set; coded frames freeze the catalog.
        if !self.tracks_have_timestamps() {
            return false;
        }
        if !self.expected_tracks_present() {
            return false;
        }
        if let Some(hint) = &self.hint {
            if expects_video(hint) && !self.has_kind(MediaKind::Video) {
                return false;
            }
            if expects_audio(hint) && !self.has_kind(MediaKind::Audio) {
                return false;
            }
            return true;
        }
        self.saw_sample || (self.has_kind(MediaKind::Video) && self.has_kind(MediaKind::Audio))
    }

    fn tracks_have_timestamps(&self) -> bool {
        self.tracks.iter().all(|track| track.first_pts.is_some())
    }

    fn expected_tracks_present(&self) -> bool {
        self.expected_audio
            .iter()
            .all(|id| self.has_source(&map::source_key(CmafCodec::Aac, Some(*id))))
            && self
                .expected_video
                .iter()
                .all(|id| self.has_source(&map::source_key(CmafCodec::Avc, Some(*id))))
    }

    fn has_source(&self, key: &SourceTrackKey) -> bool {
        self.tracks
            .iter()
            .any(|track| track.source_key.as_ref() == Some(key))
    }

    fn has_kind(&self, kind: MediaKind) -> bool {
        self.tracks.iter().any(|track| track.kind() == kind)
    }

    fn freeze(&mut self) -> Result<DiscoveryReport, SourceError> {
        let mut tracks = self.tracks.clone();
        if let Some(track) = self.text.clone() {
            tracks.push(track);
        }
        if tracks.is_empty() {
            return Err(SourceError::Demux(
                "RTMP contained no H.264, HEVC, AV1, AAC, or Opus track".into(),
            ));
        }
        let catalog = DiscoveryReport {
            tracks: TrackCatalog::new(tracks)?,
        };
        self.catalog = Some(catalog.clone());
        Ok(catalog)
    }

    fn take_catalog(&mut self) -> Option<DiscoveryReport> {
        if self.catalog.is_none() && !self.tracks.is_empty() {
            return self.freeze().ok();
        }
        self.catalog.clone()
    }

    fn finish_on_limit(
        mut self,
        problem: DiscoveryProblem,
    ) -> Result<(DiscoveryReport, VecDeque<Packet>), SourceError> {
        if self.tracks.is_empty() {
            Err(problem.into())
        } else {
            let catalog = self.freeze()?;
            Ok((catalog, std::mem::take(&mut self.prefetch)))
        }
    }
}

fn live_packets(
    event: IngressEvent,
    tracks: &LiveTracks,
    limits: InputLimits,
) -> Result<VecDeque<Packet>, SourceError> {
    match event {
        IngressEvent::Script { timestamp, payload } => {
            let Some(text) = caption::cue_text(&payload) else {
                return Ok(VecDeque::new());
            };
            let Some(track_id) = tracks.subtitle else {
                return Ok(VecDeque::new());
            };
            Ok(VecDeque::from([map::text_packet(
                track_id,
                timestamp,
                text,
                limits.maximum_payload_bytes_per_packet,
            )?]))
        }
        IngressEvent::Metadata(_) => Ok(VecDeque::new()),
        IngressEvent::End(_) | IngressEvent::Failed(_) => Err(SourceError::Input(
            "terminal RTMP event reached packet mapping".into(),
        )),
        IngressEvent::Audio { timestamp, media } => live_samples(
            timestamp,
            media.cmaf_units().map_err(demux)?,
            tracks,
            limits,
        ),
        IngressEvent::Video { timestamp, media } => live_samples(
            timestamp,
            media.cmaf_units().map_err(demux)?,
            tracks,
            limits,
        ),
    }
}

fn live_samples(
    timestamp: u32,
    units: Vec<CmafUnit>,
    tracks: &LiveTracks,
    limits: InputLimits,
) -> Result<VecDeque<Packet>, SourceError> {
    let mut packets = VecDeque::new();
    for unit in units {
        if let Some(packet) = live_sample(timestamp, unit, tracks, limits)? {
            packets.push_back(packet);
        }
    }
    Ok(packets)
}

fn live_sample(
    timestamp: u32,
    unit: CmafUnit,
    tracks: &LiveTracks,
    limits: InputLimits,
) -> Result<Option<Packet>, SourceError> {
    let key = map::source_key(unit.codec(), unit.track_id());
    match unit {
        CmafUnit::Configuration { .. } => {
            if tracks.by_source.contains_key(&key) {
                Ok(None)
            } else {
                Err(SourceError::TrackSetChanged)
            }
        }
        sample @ CmafUnit::Sample { .. } => {
            let track_id = *tracks
                .by_source
                .get(&key)
                .ok_or(SourceError::TrackSetChanged)?;
            map::packet(
                track_id,
                timestamp,
                sample,
                limits.maximum_payload_bytes_per_packet,
            )
            .map(Some)
        }
    }
}

fn numbered_track_ids<'a>(ids: impl Iterator<Item = &'a u32>) -> BTreeSet<u8> {
    ids.filter_map(|id| u8::try_from(*id).ok()).collect()
}

fn event_bytes(event: &IngressEvent) -> usize {
    match event {
        IngressEvent::Audio { media, .. } => media.raw.len(),
        IngressEvent::Video { media, .. } => media.raw.len(),
        IngressEvent::Metadata(metadata) => metadata.raw.len(),
        IngressEvent::Script { payload, .. } => payload.len(),
        IngressEvent::End(_) | IngressEvent::Failed(_) => 0,
    }
}

fn expects_video(hint: &EncoderSummary) -> bool {
    hint.video_codec.is_some() || hint.width.is_some() || hint.height.is_some()
}

fn expects_audio(hint: &EncoderSummary) -> bool {
    hint.audio_codec.is_some()
}

fn demux(error: impl std::fmt::Display) -> SourceError {
    SourceError::Demux(error.to_string().into())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use cc_rtmp::{EnhancedValidationMode, ValidatedMedia};

    use crate::{
        domain::{Codec, MediaKind},
        observe::{ProcessMeters, SessionMeters},
        source::{
            DiscoveryLimits, H264CaptionDetector, InputLimits, InputState, PacketSource,
            rtmp::{IngressEvent, channel},
        },
    };

    use super::*;

    fn discovery_limits() -> DiscoveryLimits {
        DiscoveryLimits {
            maximum_probe_bytes: 64 * 1024,
            maximum_wall_time: Duration::from_secs(2),
        }
    }

    fn audio(timestamp: u32, packet_type: u8, payload: &[u8]) -> IngressEvent {
        let mut raw = vec![0xaf, packet_type];
        raw.extend_from_slice(payload);
        IngressEvent::Audio {
            timestamp,
            media: ValidatedMedia::parse_audio(Bytes::from(raw), EnhancedValidationMode::Strict)
                .expect("legacy AAC is valid"),
        }
    }

    fn video_config(payload: &[u8]) -> IngressEvent {
        let mut raw = vec![0x17, 0x00, 0x00, 0x00, 0x00];
        raw.extend_from_slice(payload);
        IngressEvent::Video {
            timestamp: 0,
            media: ValidatedMedia::parse_video(Bytes::from(raw), EnhancedValidationMode::Strict)
                .expect("legacy AVC config is valid"),
        }
    }

    fn video_sample(timestamp: u32, payload: &[u8]) -> IngressEvent {
        let mut raw = vec![0x17, 0x01, 0x00, 0x00, 0x00];
        raw.extend_from_slice(payload);
        IngressEvent::Video {
            timestamp,
            media: ValidatedMedia::parse_video(Bytes::from(raw), EnhancedValidationMode::Strict)
                .expect("legacy AVC sample is valid"),
        }
    }

    async fn drain(source: &mut RtmpPacketSource) -> (Vec<crate::source::Packet>, InputState) {
        let mut packets = Vec::new();
        loop {
            let state = source.fill(&mut packets).await.expect("RTMP demuxes");
            if !state.is_open() {
                return (packets, state);
            }
        }
    }

    #[tokio::test]
    async fn discovers_h264_and_raw_aac() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(video_config(crate::mux::fixtures::H264_EXTRADATA))
            .await
            .expect("config queues");
        writer
            .send(audio(0, 0, crate::mux::fixtures::AAC_EXTRADATA))
            .await
            .expect("audio config queues");
        writer
            .send(video_sample(0, crate::mux::fixtures::H264_IDR))
            .await
            .expect("video queues");
        writer
            .send(audio(21, 1, crate::mux::fixtures::AAC_FRAME))
            .await
            .expect("audio queues");
        writer.finish(InputState::Closed);

        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let discovery = source.discover(discovery_limits()).await?;
        let tracks = discovery.tracks.tracks();
        assert_eq!(tracks.len(), 2);
        let video = tracks
            .iter()
            .find(|track| track.kind() == MediaKind::Video)
            .expect("video");
        let audio_track = tracks
            .iter()
            .find(|track| track.kind() == MediaKind::Audio)
            .expect("audio");
        assert_eq!(video.codec, Codec::H264);
        assert_eq!(audio_track.codec, Codec::Aac);
        assert_eq!(video.timebase, map::TIMEBASE);
        assert_eq!(audio_track.timebase, map::TIMEBASE);
        assert_eq!(video.first_pts, Some(0));
        assert_eq!(audio_track.first_pts, Some(21));
        let crate::domain::MediaParameters::Audio { frame_size, .. } = audio_track.parameters
        else {
            panic!("audio track parameters");
        };
        assert_eq!(frame_size, Some(nz::u32!(1_024)));
        assert_eq!(
            video.codec_extradata.as_bytes(),
            crate::mux::fixtures::H264_EXTRADATA
        );
        assert_eq!(
            audio_track.codec_extradata.as_bytes(),
            crate::mux::fixtures::AAC_EXTRADATA
        );

        let (packets, state) = drain(&mut source).await;
        assert_eq!(state, InputState::Closed);
        assert!(
            packets
                .iter()
                .any(|packet| packet.track_id == video.id && packet.random_access)
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.track_id == audio_track.id)
        );
        Ok(())
    }

    #[tokio::test]
    async fn caption_scanner_sees_avcc_framing() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(video_config(crate::mux::fixtures::H264_EXTRADATA))
            .await
            .expect("config queues");
        writer
            .send(video_sample(0, crate::mux::fixtures::H264_IDR))
            .await
            .expect("sample queues");
        writer.finish(InputState::Closed);

        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let discovery = source.discover(discovery_limits()).await?;
        let video = discovery
            .tracks
            .tracks()
            .iter()
            .find(|track| track.codec == Codec::H264)
            .expect("H.264");
        let mut detector =
            H264CaptionDetector::new(video).expect("avcC extradata selects AVCC framing");
        let (packets, _) = drain(&mut source).await;
        for packet in packets.iter().filter(|packet| packet.track_id == video.id) {
            let _ = detector.inspect(packet.payload.as_bytes());
        }
        assert_eq!(detector.malformed_sei(), 0);
        assert!(detector.access_units_seen() > 0);
        Ok(())
    }

    #[tokio::test]
    async fn coded_frames_before_a_sequence_header_fail_discovery() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(video_sample(0, crate::mux::fixtures::H264_IDR))
            .await
            .expect("sample queues");
        writer.finish(InputState::Closed);
        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let error = source
            .discover(discovery_limits())
            .await
            .expect_err("coded frames need a sequence header");
        assert!(error.to_string().contains("sequence header"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn fill_before_discover_is_refused() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        drop(writer);
        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let error = source
            .fill(&mut Vec::new())
            .await
            .expect_err("fill requires discovery");
        assert!(matches!(error, SourceError::Input(_)));
        Ok(())
    }

    fn script(timestamp: u32, name: &[u8], text: &[u8]) -> IngressEvent {
        IngressEvent::Script {
            timestamp,
            payload: crate::source::encode_cue(name, text),
        }
    }

    #[tokio::test]
    async fn discovers_script_captions_already_queued_with_the_first_sample()
    -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(audio(0, 0, crate::mux::fixtures::AAC_EXTRADATA))
            .await
            .expect("audio config queues");
        writer
            .send(audio(0, 1, crate::mux::fixtures::AAC_FRAME))
            .await
            .expect("audio queues");
        writer
            .send(script(40, b"onCaption", b"hello"))
            .await
            .expect("caption queues");
        writer.finish(InputState::Closed);

        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let discovery = source.discover(discovery_limits()).await?;
        let text = discovery
            .tracks
            .tracks()
            .iter()
            .find(|track| track.kind() == MediaKind::Subtitle)
            .expect("script-data captions join the catalog");
        assert_eq!(text.codec, Codec::Text);
        assert_eq!(text.timebase, map::TIMEBASE);
        assert_eq!(text.first_pts, Some(40));

        let (packets, state) = drain(&mut source).await;
        assert_eq!(state, InputState::Closed);
        let caption = packets
            .iter()
            .find(|packet| packet.track_id == text.id)
            .expect("caption packet");
        assert_eq!(caption.pts, Some(40));
        assert_eq!(caption.duration, None);
        assert_eq!(caption.payload.as_bytes(), b"hello");
        Ok(())
    }

    #[tokio::test]
    async fn unknown_script_names_do_not_create_a_text_track() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(audio(0, 0, crate::mux::fixtures::AAC_EXTRADATA))
            .await
            .expect("audio config queues");
        writer
            .send(audio(0, 1, crate::mux::fixtures::AAC_FRAME))
            .await
            .expect("audio queues");
        writer
            .send(script(40, b"onCuePoint", b"hello"))
            .await
            .expect("unknown script queues");
        writer.finish(InputState::Closed);

        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let discovery = source.discover(discovery_limits()).await?;
        assert!(
            discovery
                .tracks
                .tracks()
                .iter()
                .all(|track| track.kind() != MediaKind::Subtitle),
            "unknown script names are not a subtitle track"
        );
        Ok(())
    }

    #[tokio::test]
    async fn late_captions_without_a_text_track_are_ignored() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(audio(0, 0, crate::mux::fixtures::AAC_EXTRADATA))
            .await
            .expect("audio config queues");
        writer
            .send(audio(0, 1, crate::mux::fixtures::AAC_FRAME))
            .await
            .expect("audio queues");

        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let discovery = source.discover(discovery_limits()).await?;
        assert!(
            discovery
                .tracks
                .tracks()
                .iter()
                .all(|track| track.kind() != MediaKind::Subtitle)
        );

        writer
            .send(script(40, b"onCaption", b"too late"))
            .await
            .expect("late caption queues");
        writer
            .send(audio(21, 1, crate::mux::fixtures::AAC_FRAME))
            .await
            .expect("audio queues");
        writer.finish(InputState::Closed);

        let (packets, state) = drain(&mut source).await;
        assert_eq!(state, InputState::Closed);
        assert!(
            packets
                .iter()
                .all(|packet| packet.payload.as_bytes() != b"too late"),
            "late captions without a text track must not become packets"
        );
        Ok(())
    }

    #[tokio::test]
    async fn probe_budget_without_tracks_is_a_discovery_error() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(video_config(crate::mux::fixtures::H264_EXTRADATA))
            .await
            .expect("config queues");
        writer.finish(InputState::Closed);
        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let error = source
            .discover(DiscoveryLimits {
                maximum_probe_bytes: 8,
                maximum_wall_time: Duration::from_secs(2),
            })
            .await
            .expect_err("a truncated probe cannot resolve tracks");
        assert!(
            matches!(
                error,
                SourceError::Discovery(DiscoveryProblem::ProbeLimitExceeded)
            ),
            "{error}"
        );
        Ok(())
    }

    fn one_track_audio(
        timestamp: u32,
        packet_type: u8,
        track_id: u8,
        payload: &[u8],
    ) -> IngressEvent {
        let mut raw = vec![0x95, packet_type, b'm', b'p', b'4', b'a', track_id];
        raw.extend_from_slice(payload);
        IngressEvent::Audio {
            timestamp,
            media: ValidatedMedia::parse_audio(Bytes::from(raw), EnhancedValidationMode::Strict)
                .expect("OneTrack AAC is valid"),
        }
    }

    fn packed_audio(timestamp: u32, packet_type: u8, tracks: &[(u8, &[u8])]) -> IngressEvent {
        let mut raw = vec![0x95, 0x10 | packet_type, b'm', b'p', b'4', b'a'];
        for (id, payload) in tracks {
            raw.push(*id);
            let len = u32::try_from(payload.len()).expect("fixture payload fits u24");
            raw.extend_from_slice(&len.to_be_bytes()[1..]);
            raw.extend_from_slice(payload);
        }
        IngressEvent::Audio {
            timestamp,
            media: ValidatedMedia::parse_audio(Bytes::from(raw), EnhancedValidationMode::Strict)
                .expect("ManyTracks AAC is valid"),
        }
    }

    fn one_track_video(
        timestamp: u32,
        packet_type: u8,
        track_id: u8,
        payload: &[u8],
    ) -> IngressEvent {
        let mut raw = vec![0x96, packet_type, b'a', b'v', b'c', b'1', track_id];
        if packet_type == 1 {
            raw.extend_from_slice(&[0, 0, 0]);
        }
        raw.extend_from_slice(payload);
        IngressEvent::Video {
            timestamp,
            media: ValidatedMedia::parse_video(Bytes::from(raw), EnhancedValidationMode::Strict)
                .expect("OneTrack AVC is valid"),
        }
    }

    async fn source_from(events: Vec<IngressEvent>) -> Result<RtmpPacketSource, SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        for event in events {
            writer.send(event).await.expect("event queues");
        }
        writer.finish(InputState::Closed);
        RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())
    }

    #[tokio::test]
    async fn discovers_two_one_track_aac_languages() -> Result<(), SourceError> {
        let mut source = source_from(vec![
            one_track_audio(0, 0, 1, crate::mux::fixtures::AAC_EXTRADATA),
            one_track_audio(0, 0, 2, crate::mux::fixtures::AAC_EXTRADATA),
            one_track_audio(0, 1, 1, crate::mux::fixtures::AAC_FRAME),
            one_track_audio(21, 1, 2, crate::mux::fixtures::AAC_FRAME),
        ])
        .await?;
        let discovery = source.discover(discovery_limits()).await?;
        let audio: Vec<_> = discovery
            .tracks
            .tracks()
            .iter()
            .filter(|track| track.kind() == MediaKind::Audio)
            .collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(
            audio[0].source_key.as_ref().map(|key| key.0.as_ref()),
            Some("audio/1")
        );
        assert_eq!(
            audio[1].source_key.as_ref().map(|key| key.0.as_ref()),
            Some("audio/2")
        );
        assert_eq!(audio[0].first_pts, Some(0));
        assert_eq!(audio[1].first_pts, Some(21));

        let (packets, state) = drain(&mut source).await;
        assert_eq!(state, InputState::Closed);
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.track_id == audio[0].id)
                .count(),
            1
        );
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.track_id == audio[1].id)
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn packed_many_tracks_yields_a_packet_per_sibling() -> Result<(), SourceError> {
        let mut source = source_from(vec![
            packed_audio(
                0,
                0,
                &[
                    (1, crate::mux::fixtures::AAC_EXTRADATA),
                    (3, crate::mux::fixtures::AAC_EXTRADATA),
                ],
            ),
            packed_audio(
                0,
                1,
                &[
                    (1, crate::mux::fixtures::AAC_FRAME),
                    (3, crate::mux::fixtures::AAC_FRAME),
                ],
            ),
        ])
        .await?;
        let discovery = source.discover(discovery_limits()).await?;
        let audio: Vec<_> = discovery
            .tracks
            .tracks()
            .iter()
            .filter(|track| track.kind() == MediaKind::Audio)
            .collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(
            audio[0].source_key.as_ref().map(|key| key.0.as_ref()),
            Some("audio/1")
        );
        assert_eq!(
            audio[1].source_key.as_ref().map(|key| key.0.as_ref()),
            Some("audio/3")
        );

        let (packets, _) = drain(&mut source).await;
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].track_id, audio[0].id);
        assert_eq!(packets[1].track_id, audio[1].id);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_default_audio_and_one_track_extra_are_distinct() -> Result<(), SourceError> {
        let mut source = source_from(vec![
            audio(0, 0, crate::mux::fixtures::AAC_EXTRADATA),
            one_track_audio(0, 0, 1, crate::mux::fixtures::AAC_EXTRADATA),
            audio(0, 1, crate::mux::fixtures::AAC_FRAME),
            one_track_audio(21, 1, 1, crate::mux::fixtures::AAC_FRAME),
        ])
        .await?;
        let discovery = source.discover(discovery_limits()).await?;
        let keys: Vec<_> = discovery
            .tracks
            .tracks()
            .iter()
            .filter_map(|track| track.source_key.as_ref().map(|key| key.0.as_ref()))
            .collect();
        assert_eq!(keys, ["audio", "audio/1"]);
        Ok(())
    }

    #[tokio::test]
    async fn discovers_two_one_track_video_angles() -> Result<(), SourceError> {
        let mut source = source_from(vec![
            one_track_video(0, 0, 1, crate::mux::fixtures::H264_EXTRADATA),
            one_track_video(0, 0, 2, crate::mux::fixtures::H264_EXTRADATA),
            one_track_video(0, 1, 1, crate::mux::fixtures::H264_IDR),
            one_track_video(40, 1, 2, crate::mux::fixtures::H264_IDR),
        ])
        .await?;
        let discovery = source.discover(discovery_limits()).await?;
        let video: Vec<_> = discovery
            .tracks
            .tracks()
            .iter()
            .filter(|track| track.kind() == MediaKind::Video)
            .collect();
        assert_eq!(video.len(), 2);
        assert_eq!(
            video[0].source_key.as_ref().map(|key| key.0.as_ref()),
            Some("video/1")
        );
        assert_eq!(
            video[1].source_key.as_ref().map(|key| key.0.as_ref()),
            Some("video/2")
        );
        assert_eq!(video[0].first_pts, Some(0));
        assert_eq!(video[1].first_pts, Some(40));
        Ok(())
    }

    #[tokio::test]
    async fn a_new_track_after_freeze_is_track_set_changed() -> Result<(), SourceError> {
        let meters = SessionMeters::new(ProcessMeters::default());
        let (reader, writer) = channel(nz::usize!(64 * 1024));
        writer
            .send(one_track_audio(
                0,
                0,
                1,
                crate::mux::fixtures::AAC_EXTRADATA,
            ))
            .await
            .expect("config queues");
        writer
            .send(one_track_audio(0, 1, 1, crate::mux::fixtures::AAC_FRAME))
            .await
            .expect("sample queues");

        let mut source =
            RtmpPacketSource::new(reader, InputLimits::permissive(), meters.source_view())?;
        let discovery = source.discover(discovery_limits()).await?;
        assert_eq!(discovery.tracks.tracks().len(), 1);

        writer
            .send(one_track_audio(
                0,
                0,
                2,
                crate::mux::fixtures::AAC_EXTRADATA,
            ))
            .await
            .expect("late track queues");
        writer.finish(InputState::Closed);

        let error = source
            .fill(&mut Vec::new())
            .await
            .expect_err("a new Enhanced track after freeze is not admitted");
        assert!(matches!(error, SourceError::TrackSetChanged), "{error}");
        Ok(())
    }
}
