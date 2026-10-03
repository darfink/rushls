//! Streaming Hang media behind a bounded `PacketSource`.
//!
//! Discovery fixes the track set, accepts provisional Opus configuration, and
//! waits until each track has a presentation timestamp. After discovery, an
//! added or removed track or a codec-config change is fatal. Media is read in
//! group order and never silently skipped: a sequence gap fails the publication.
//!
//! Text tracks are the exception to waiting: captions may stay silent for
//! minutes, so discovery freezes on audio and video alone and cues join
//! whenever the publisher sends one.

use std::{collections::VecDeque, sync::Arc, task::Poll, time::Instant};

use crate::{
    domain::{Appender, BoxFuture, Codec, DiscoveredTrack, MediaKind, TrackCatalog, TrackId},
    observe::SourceMeters,
    source::{
        DiscoveryLimits, DiscoveryProblem, DiscoveryReport, InputLimits, InputState, Packet,
        PacketSource, SourceError, record_first_pts,
    },
};

use super::{
    ReadError, catalog, loc,
    map::{self, CatalogFingerprint, MappedCatalog},
};

pub struct MoqPacketSource {
    limits: InputLimits,
    meters: Arc<dyn SourceMeters>,
    discovery: Option<DiscoveryReport>,
    fingerprint: Option<CatalogFingerprint>,
    broadcast: Option<moq_net::broadcast::Consumer>,
    catalog: Option<catalog::Reader>,
    tracks: Vec<LiveTrack>,
    /// Where announced broadcasts are resolved.
    origin: Option<moq_net::origin::Consumer>,
    announced: Option<moq_net::announce::Consumer>,
    broadcast_path: Option<String>,
    /// Held so the transport stays open for as long as this source does.
    _session: Option<moq_net::Session>,
    prefetch: VecDeque<Packet>,
    terminal: Option<InputState>,
    next_track: usize,
    /// The receive cache's bytes, mirrored into the publisher budget.
    cache: Option<CacheCharge>,
    /// Where the audio/video timeline starts, once discovery has frozen it.
    cue_floor: Option<i64>,
}

/// Charges `moq_net`'s receive cache to the publisher budget.
///
/// The dependency allocates each frame itself, before this source sees it, and
/// keeps recent groups for late readers. Its pool target is an eviction goal,
/// not a limit, so without this those bytes would sit outside
/// `memory.per_publisher` entirely. The charge follows the pool's own count,
/// taken each time a frame is read: it lags one frame, which is as early as
/// anything outside the dependency can see an allocation, and a budget that
/// cannot cover it ends the publication like any other exhausted stage.
///
/// A frame both cached and in the pipeline is charged twice, once as cache and
/// once as a packet. The pool's target bounds that overlap, and it errs toward
/// refusing a publisher rather than under-counting one.
struct CacheCharge {
    pool: moq_net::cache::Pool,
    held: Option<crate::domain::Reservation>,
}

impl CacheCharge {
    fn reconcile(
        &mut self,
        budget: Option<&crate::domain::PipelineBudget>,
    ) -> Result<(), crate::domain::BudgetExceeded> {
        let Some(budget) = budget else {
            return Ok(());
        };
        let used = usize::try_from(self.pool.used()).unwrap_or(usize::MAX);
        let held = self
            .held
            .as_ref()
            .map_or(0, crate::domain::Reservation::bytes);
        if used > held {
            let more = budget.try_reserve(used - held, crate::domain::Stage::MoqCache)?;
            match &mut self.held {
                Some(held) => held.absorb(more),
                None => self.held = Some(more),
            }
        } else if let Some(reservation) = self.held.as_mut().filter(|_| used < held) {
            drop(reservation.split(held - used));
        }
        Ok(())
    }
}

struct LiveTrack {
    id: TrackId,
    codec: crate::domain::Codec,
    reader: loc::Reader,
    ended: bool,
    inline: Option<super::h264::Inline>,
}

impl MoqPacketSource {
    /// In-process broadcasts, used by tests that must not open a QUIC socket.
    #[cfg(test)]
    pub fn from_broadcast(
        broadcast: moq_net::broadcast::Consumer,
        limits: InputLimits,
        meters: Arc<dyn SourceMeters>,
    ) -> Result<Self, SourceError> {
        limits.validate()?;
        Ok(Self {
            broadcast: Some(broadcast),
            ..Self::empty(limits, meters)
        })
    }

    pub fn from_origin(
        origin: &moq_net::origin::Consumer,
        session: Option<moq_net::Session>,
        pool: moq_net::cache::Pool,
        limits: InputLimits,
        meters: Arc<dyn SourceMeters>,
    ) -> Result<Self, SourceError> {
        limits.validate()?;
        Ok(Self {
            announced: Some(origin.announced()),
            origin: Some(origin.clone()),
            _session: session,
            cache: Some(CacheCharge { pool, held: None }),
            ..Self::empty(limits, meters)
        })
    }

    fn empty(limits: InputLimits, meters: Arc<dyn SourceMeters>) -> Self {
        Self {
            limits,
            meters,
            discovery: None,
            fingerprint: None,
            broadcast: None,
            catalog: None,
            tracks: Vec::new(),
            origin: None,
            announced: None,
            broadcast_path: None,
            _session: None,
            prefetch: VecDeque::new(),
            terminal: None,
            next_track: 0,
            cache: None,
            cue_floor: None,
        }
    }

    /// Brings the cache charge up to date; called for every frame read.
    fn charge_cache(&mut self) -> Result<(), SourceError> {
        if let Some(cache) = &mut self.cache {
            cache.reconcile(self.meters.pipeline_budget())?;
        }
        Ok(())
    }

    async fn ensure_catalog_subscription(&mut self) -> Result<(), SourceError> {
        if self.catalog.is_some() {
            return Ok(());
        }
        if self.broadcast.is_none() {
            let broadcast = self.wait_broadcast().await?;
            self.broadcast = Some(broadcast);
        }
        let broadcast = self.broadcast.as_ref().expect("broadcast just stored");
        let track = broadcast.track(catalog::TRACK_NAME).map_err(classify_net)?;
        let subscriber = track
            .subscribe(catalog::Reader::subscription())
            .await
            .map_err(classify_net)?;
        self.catalog = Some(catalog::Reader::new(
            subscriber,
            self.limits.maximum_payload_bytes_per_packet,
        ));
        Ok(())
    }

    async fn wait_broadcast(&mut self) -> Result<moq_net::broadcast::Consumer, SourceError> {
        let announced = self.announced.as_mut().ok_or_else(|| {
            SourceError::Input("MOQ ingest has no broadcast and no origin to wait on".into())
        })?;
        let path = loop {
            let Some(announce) = announced.next().await else {
                return Err(DiscoveryProblem::Abandoned.into());
            };
            // An announcement is a route to a prefix, and a broadcast announces
            // its own path; the origin resolves that path to the broadcast.
            if announce.kind.is_active() {
                break announce.prefix.to_string();
            }
        };
        let origin = self
            .origin
            .as_ref()
            .expect("an announce cursor comes from an origin");
        let broadcast = origin
            .routed_broadcast(path.as_str())
            .await
            .map_err(classify_net)?;
        self.broadcast_path = Some(path);
        Ok(broadcast)
    }

    async fn subscribe_tracks(&mut self, mapped: &MappedCatalog) -> Result<(), SourceError> {
        let broadcast = self.broadcast.as_ref().ok_or_else(|| {
            SourceError::Input("cannot subscribe tracks before a broadcast is announced".into())
        })?;
        self.tracks.clear();
        for (index, track) in mapped.tracks.iter().enumerate() {
            let name = rendition_name(track)?;
            let consumer = broadcast.track(name).map_err(classify_net)?;
            let subscriber = consumer
                .subscribe(loc::Reader::subscription())
                .await
                .map_err(classify_net)?;
            let reader = loc::Reader::new(subscriber, mapped.legacy[index]);
            self.tracks.push(LiveTrack {
                id: track.id,
                codec: track.codec,
                reader: if track.codec == Codec::Text {
                    reader.cues()
                } else {
                    reader
                },
                ended: false,
                inline: mapped.inline_h264[index].then(super::h264::Inline::default),
            });
        }
        Ok(())
    }

    fn media_packet(&mut self, index: usize, mut frame: loc::Frame) -> Result<Packet, SourceError> {
        self.charge_cache()?;
        let track = self
            .tracks
            .get_mut(index)
            .ok_or_else(|| SourceError::Demux("MOQ frame for an unknown track".into()))?;
        if let Some(inline) = &mut track.inline {
            inline.convert(
                &mut frame,
                track.id,
                self.limits.maximum_payload_bytes_per_packet,
            )?;
        }
        let mut packet = map::packet(
            track.id,
            track.codec,
            &frame,
            self.limits.maximum_payload_bytes_per_packet,
        )?;
        if track.codec == Codec::Text {
            clamp_cue(&mut packet, self.cue_floor);
        }
        packet.account(self.meters.pipeline_budget())?;
        Ok(packet)
    }

    fn check_announcement(&self, path: &str, present: bool) -> Result<(), SourceError> {
        if present
            && self
                .broadcast_path
                .as_ref()
                .is_some_and(|expected| expected != path)
        {
            return Err(SourceError::TrackSetChanged);
        }
        Ok(())
    }

    /// Refinement can change codec priming; recompute the audible start from
    /// retained packets before any discovery metadata escapes to normalization.
    fn restore_first_pts(&self, mapped: &mut MappedCatalog) -> Result<(), SourceError> {
        for packet in &self.prefetch {
            let track = mapped
                .tracks
                .iter_mut()
                .find(|track| track.id == packet.track_id)
                .expect("prefetched packets belong to discovered tracks");
            record_first_pts(track, packet)?;
        }
        Ok(())
    }

    fn store_discovery(&mut self, mut mapped: MappedCatalog) -> DiscoveryReport {
        // Audio and video all have a first timestamp by now, and the earliest
        // of them is where the presentation starts.
        self.cue_floor = mapped
            .tracks
            .iter()
            .filter(|track| track.kind() != MediaKind::Subtitle)
            .filter_map(|track| track.first_pts)
            .min();
        let floor = self.cue_floor;
        for track in mapped
            .tracks
            .iter_mut()
            .filter(|track| track.codec == Codec::Text)
        {
            track.first_pts = track
                .first_pts
                .map(|pts| floor.map_or(pts, |floor| pts.max(floor)));
        }
        for packet in &mut self.prefetch {
            let cue = mapped
                .tracks
                .iter()
                .any(|track| track.id == packet.track_id && track.codec == Codec::Text);
            if cue {
                clamp_cue(packet, self.cue_floor);
            }
        }
        self.fingerprint = Some(mapped.fingerprint);
        let catalog = DiscoveryReport {
            tracks: TrackCatalog::new(mapped.tracks)
                .expect("mapped catalogs are non-empty and unique"),
        };
        self.discovery = Some(catalog.clone());
        catalog
    }
}

impl PacketSource for MoqPacketSource {
    fn discover(
        &mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        Box::pin(async move {
            if let Some(discovery) = &self.discovery {
                return Ok(discovery.clone());
            }
            limits.validate()?;

            let deadline = Instant::now() + limits.maximum_wall_time;
            tokio::time::timeout(limits.maximum_wall_time, self.ensure_catalog_subscription())
                .await
                .map_err(|_| DiscoveryProblem::DeadlineExceeded)??;
            let mut probed = 0_usize;
            let mut mapped: Option<MappedCatalog> = None;

            loop {
                if Instant::now() >= deadline {
                    return Err(DiscoveryProblem::DeadlineExceeded.into());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let incoming = match tokio::time::timeout(remaining, self.next_incoming()).await {
                    Ok(incoming) => incoming?,
                    Err(_) => return Err(DiscoveryProblem::DeadlineExceeded.into()),
                };
                match incoming {
                    Incoming::Catalog(Some(catalog)) => {
                        probed = probed.saturating_add(catalog.wire_bytes);
                        if probed > limits.maximum_probe_bytes {
                            return Err(DiscoveryProblem::ProbeLimitExceeded.into());
                        }
                        let next = map::tracks_from_catalog(
                            &catalog.video.renditions,
                            &catalog.audio.renditions,
                            &catalog.text.renditions,
                        )?;
                        if let Some(existing) = &mut mapped {
                            existing.refine(next)?;
                            self.restore_first_pts(existing)?;
                        } else {
                            tokio::time::timeout(
                                deadline.saturating_duration_since(Instant::now()),
                                self.subscribe_tracks(&next),
                            )
                            .await
                            .map_err(|_| DiscoveryProblem::DeadlineExceeded)??;
                            mapped = Some(next);
                        }
                    }
                    Incoming::Catalog(None) => {
                        if mapped.is_none() {
                            return Err(DiscoveryProblem::Abandoned.into());
                        }
                    }
                    Incoming::Frame { index, mut frame } => {
                        self.charge_cache()?;
                        probed = probed.saturating_add(frame.payload.len());
                        if probed > limits.maximum_probe_bytes {
                            return Err(DiscoveryProblem::ProbeLimitExceeded.into());
                        }
                        let Some(mapped) = mapped.as_mut() else {
                            return Err(SourceError::Demux(
                                "LOC media arrived before a hang catalog".into(),
                            ));
                        };
                        let track = mapped.tracks.get_mut(index).ok_or_else(|| {
                            SourceError::Demux("LOC frame for an unknown track".into())
                        })?;
                        if let Some(inline) = &mut self.tracks[index].inline {
                            inline.convert(
                                &mut frame,
                                track.id,
                                self.limits.maximum_payload_bytes_per_packet,
                            )?;
                            inline.configure(track)?;
                        }
                        let mut packet = map::packet(
                            track.id,
                            track.codec,
                            &frame,
                            self.limits.maximum_payload_bytes_per_packet,
                        )?;
                        packet.account(self.meters.pipeline_budget())?;
                        record_first_pts(track, &packet)?;
                        self.prefetch.push_back(packet);
                        if can_freeze(mapped) {
                            return Ok(self.store_discovery(mapped.clone()));
                        }
                    }
                    Incoming::TrackEnded => {
                        if self.tracks.iter().any(|track| track.ended) {
                            return Err(DiscoveryProblem::Abandoned.into());
                        }
                    }
                    Incoming::Announce { path, present } => {
                        self.check_announcement(&path, present)?;
                    }
                    Incoming::Closed(state) => {
                        self.terminal = Some(state);
                        return Err(DiscoveryProblem::Abandoned.into());
                    }
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
                    "MOQ source must be discovered before reading packets".into(),
                ));
            }

            let mut packets = 0_usize;
            let mut payload_bytes = 0_usize;
            loop {
                let packet = if let Some(packet) = self.prefetch.pop_front() {
                    packet
                } else if self.terminal.is_some() {
                    break;
                } else {
                    let incoming = kio::wait(|waiter| match self.poll_incoming(waiter) {
                        Poll::Pending if packets > 0 => Poll::Ready(Ok(None)),
                        other => other.map(|result| result.map(Some)),
                    })
                    .await?;
                    let Some(incoming) = incoming else {
                        break;
                    };
                    match incoming {
                        Incoming::Frame { index, frame } => self.media_packet(index, frame)?,
                        Incoming::Catalog(Some(catalog)) => {
                            let next = map::tracks_from_catalog(
                                &catalog.video.renditions,
                                &catalog.audio.renditions,
                                &catalog.text.renditions,
                            )?;
                            if let Some(frozen) = &self.fingerprint {
                                frozen.diff(&next.fingerprint)?;
                            }
                            continue;
                        }
                        Incoming::Catalog(None) => continue,
                        Incoming::TrackEnded => {
                            if self.tracks.iter().all(|track| track.ended) {
                                self.terminal = Some(InputState::Closed);
                                break;
                            }
                            continue;
                        }
                        Incoming::Announce { path, present } => {
                            self.check_announcement(&path, present)?;
                            continue;
                        }
                        Incoming::Closed(state) => {
                            self.terminal = Some(state);
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
                    self.prefetch.push_front(packet);
                    break;
                }
                payload_bytes = next_bytes;
                packets += 1;
                out.push(packet);
                if packets == self.limits.maximum_packets_per_batch
                    || payload_bytes == self.limits.maximum_payload_bytes_per_batch
                {
                    break;
                }
            }

            self.meters
                .source_progress(payload_bytes as u64, packets as u64);
            Ok(if self.prefetch.is_empty() {
                self.terminal.unwrap_or(InputState::Open)
            } else {
                InputState::Open
            })
        })
    }
}

enum Incoming {
    Catalog(Option<catalog::Catalog>),
    Frame { index: usize, frame: loc::Frame },
    TrackEnded,
    Announce { path: String, present: bool },
    Closed(InputState),
}

impl MoqPacketSource {
    async fn next_incoming(&mut self) -> Result<Incoming, SourceError> {
        kio::wait(|waiter| self.poll_incoming(waiter)).await
    }

    fn poll_incoming(&mut self, waiter: &kio::Waiter) -> Poll<Result<Incoming, SourceError>> {
        if let Some(catalog) = &mut self.catalog {
            match catalog.poll_next(waiter) {
                Poll::Ready(Ok(Some(catalog))) => {
                    return Poll::Ready(Ok(Incoming::Catalog(Some(catalog))));
                }
                Poll::Ready(Ok(None)) => {
                    self.catalog = None;
                    return Poll::Ready(Ok(Incoming::Catalog(None)));
                }
                Poll::Ready(Err(error)) => return Poll::Ready(terminal_or_fail(error)),
                Poll::Pending => {}
            }
        }

        if let Some(announced) = &mut self.announced {
            match announced.poll_next(waiter) {
                Poll::Ready(Some(announce)) => {
                    return Poll::Ready(Ok(Incoming::Announce {
                        path: announce.prefix.to_string(),
                        present: announce.kind.is_active(),
                    }));
                }
                Poll::Ready(None) => {
                    self.announced = None;
                }
                Poll::Pending => {}
            }
        }

        let mut pending = false;
        let mut live = 0_usize;
        for offset in 0..self.tracks.len() {
            let index = (self.next_track + offset) % self.tracks.len();
            if self.tracks[index].ended {
                continue;
            }
            live += 1;
            match self.tracks[index].reader.poll_read(waiter) {
                Poll::Ready(Ok(Some(frame))) => {
                    self.next_track = (index + 1) % self.tracks.len();
                    return Poll::Ready(Ok(Incoming::Frame { index, frame }));
                }
                Poll::Ready(Ok(None)) => {
                    self.tracks[index].ended = true;
                    return Poll::Ready(Ok(Incoming::TrackEnded));
                }
                Poll::Ready(Err(error)) => return Poll::Ready(terminal_or_fail(error)),
                Poll::Pending => pending = true,
            }
        }

        if live == 0 && self.tracks.is_empty() && self.catalog.is_none() && self.announced.is_none()
        {
            return Poll::Ready(Ok(Incoming::Closed(InputState::Interrupted)));
        }
        if pending || live > 0 || self.catalog.is_some() || self.announced.is_some() {
            return Poll::Pending;
        }
        Poll::Ready(Ok(Incoming::Closed(InputState::Closed)))
    }
}

fn can_freeze(mapped: &MappedCatalog) -> bool {
    mapped
        .tracks
        .iter()
        .filter(|track| track.kind() != MediaKind::Subtitle)
        .all(|track| track.first_pts.is_some() && !track.codec_extradata.is_empty())
}

/// Moves a cue that starts before the presentation up to its first instant.
///
/// A subscription begins at each track's latest group. For audio and video
/// that is a recent keyframe, but for captions it is the last cue sent, which
/// can be minutes old and still on screen. That cue is still what a viewer
/// joining now should see, so it starts where the presentation starts rather
/// than failing the WebVTT writer for predating it. A clear is moved the same
/// way and simply ends nothing.
fn clamp_cue(packet: &mut Packet, floor: Option<i64>) {
    if let (Some(pts), Some(floor)) = (packet.pts, floor)
        && pts < floor
    {
        packet.pts = Some(floor);
    }
}

fn rendition_name(track: &DiscoveredTrack) -> Result<&str, SourceError> {
    let key = track
        .source_key
        .as_ref()
        .ok_or_else(|| SourceError::Demux("a mapped MOQ track has no source key".into()))?;
    key.0
        .split_once('/')
        .map(|(_, name)| name)
        .ok_or_else(|| SourceError::Demux("a mapped MOQ track has a malformed source key".into()))
}

/// A transport error raised before any track is being read.
///
/// Routed through [`ReadError`] so subscribing and reading describe a failed
/// publication the same way rather than drifting into two vocabularies.
fn classify_net(error: moq_net::Error) -> SourceError {
    classify_read(ReadError::Moq(error))
}
fn classify_read(error: ReadError) -> SourceError {
    match error {
        ReadError::Moq(inner) => SourceError::Input(inner.to_string().into()),
        ReadError::Malformed(reason) => SourceError::Demux(reason),
    }
}

/// Whether a failed read ends the publication or fails it.
///
/// A publisher going away is how a live stream normally ends, so those errors
/// become a terminal state rather than an ingest failure. Bytes this origin
/// cannot parse are the publisher's fault and stay an error.
fn terminal_or_fail(error: ReadError) -> Result<Incoming, SourceError> {
    match error {
        ReadError::Moq(moq_net::Error::Cancel | moq_net::Error::Closed) => {
            Ok(Incoming::Closed(InputState::Closed))
        }
        ReadError::Moq(
            moq_net::Error::Dropped
            | moq_net::Error::Timeout
            | moq_net::Error::Transport(_)
            | moq_net::Error::Remote(_),
        ) => Ok(Incoming::Closed(InputState::Interrupted)),
        other => Err(classify_read(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::source::DiscoveryLimits;

    use super::super::fixtures::{Fixture, captioned_catalog, discovery_limits, loc_catalog};

    #[tokio::test]
    async fn discovers_loc_tracks_and_prefills_the_first_frames() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);

        let discovery = source.discover(discovery_limits()).await?;
        assert_eq!(discovery.tracks.tracks().len(), 2);
        assert_eq!(
            discovery.tracks.tracks()[0].codec,
            crate::domain::Codec::H264
        );
        assert_eq!(discovery.tracks.tracks()[0].first_pts, Some(0));

        // A finished track also lets fill report a terminal input state.
        fixture.finish_media();
        let mut packets = Vec::new();
        let state = source.fill(&mut packets).await?;
        assert_eq!(packets.len(), 2);
        assert!(matches!(state, InputState::Open | InputState::Closed));
        Ok(())
    }

    #[tokio::test]
    async fn a_catalog_mutation_after_freeze_is_track_set_changed() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);
        source.discover(discovery_limits()).await?;

        let mut extra = loc_catalog();
        let rendition = extra["video"]["renditions"]["1080p"].clone();
        extra["video"]["renditions"]["720p"] = rendition;
        fixture.publish_catalog(&extra);

        let error = source
            .fill(&mut Vec::new())
            .await
            .expect_err("a new rendition after freeze is fatal");
        assert!(matches!(error, SourceError::TrackSetChanged));
        Ok(())
    }

    /// Real IDR slice preceded by an ATSC A/53 SEI carrying CEA-608 "HI" on
    /// field 1, as an encoder embeds captions in each access unit.
    fn captioned_idr() -> Vec<u8> {
        let payload: &[u8] = &[
            0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0xC1, 0xFF, 0xFC, 0xC8, 0x49, 0xFF,
        ];
        let mut sei = vec![0x06, 0x04, u8::try_from(payload.len()).expect("fits")];
        sei.extend_from_slice(payload);
        sei.push(0x80);
        let mut unit = u32::try_from(sei.len())
            .expect("fits")
            .to_be_bytes()
            .to_vec();
        unit.extend_from_slice(&sei);
        unit.extend_from_slice(crate::mux::fixtures::H264_IDR);
        unit
    }

    #[tokio::test]
    async fn embedded_cea608_survives_moq_ingest_and_is_declared() -> Result<(), SourceError> {
        use crate::{domain::MediaKind, mux::CaptionChannel};

        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        for index in 0..3 {
            fixture.publish_frame("1080p", index * 500_000, &captioned_idr());
        }
        fixture.publish_frame("opus", 0, &[0xFC]);
        let discovery = source.discover(discovery_limits()).await?;
        fixture.finish_media();
        fixture.producer.close();

        let mut packets = Vec::new();
        while source.fill(&mut packets).await? == InputState::Open {}

        // The same verifier a session runs, over what the MoQ source handed on.
        let tracks = discovery.tracks.tracks();
        let video = tracks
            .iter()
            .find(|track| track.kind() == MediaKind::Video)
            .expect("a video track")
            .id;
        let mut verifier = crate::media::CaptionVerifier::new(tracks, None);
        let declared = packets
            .iter()
            .filter(|packet| packet.track_id == video)
            .filter_map(|packet| verifier.inspect(video, packet.payload.as_bytes()))
            .last()
            .expect("the captions are declared");
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].channel, CaptionChannel::Cea608Field(0));
        Ok(())
    }

    /// The caption track's packets, in the order the source handed them on.
    fn cues(discovery: &DiscoveryReport, packets: &[Packet]) -> Vec<(Option<i64>, Vec<u8>)> {
        let text = discovery
            .tracks
            .tracks()
            .iter()
            .find(|track| track.codec == Codec::Text)
            .expect("the caption rendition is a track")
            .id;
        packets
            .iter()
            .filter(|packet| packet.track_id == text)
            .map(|packet| (packet.pts, packet.payload.as_bytes().to_vec()))
            .collect()
    }

    #[tokio::test]
    async fn a_silent_caption_track_does_not_hold_discovery() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&captioned_catalog());
        // Created but never written to, as a publisher serves any catalog track.
        fixture.track("captions");
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);

        // No cue has been sent, yet the catalog freezes with the text track in it.
        let discovery = source.discover(discovery_limits()).await?;
        let text = discovery
            .tracks
            .tracks()
            .iter()
            .find(|track| track.kind() == MediaKind::Subtitle)
            .expect("the caption rendition is a track");
        assert_eq!((text.codec, text.first_pts), (Codec::Text, None));
        assert_eq!(text.title.as_deref(), Some("English"));

        // Cues arrive later on the shared microsecond clock, and an empty cue
        // survives as the instruction to clear the display.
        fixture.publish_legacy_frame("captions", 1_000_000, b"hello");
        fixture.publish_legacy_frame("captions", 2_500_000, b"");
        fixture.finish_media();
        fixture.producer.close();
        let mut packets = Vec::new();
        while source.fill(&mut packets).await? == InputState::Open {}
        assert_eq!(
            cues(&discovery, &packets),
            vec![
                (Some(1_000_000), b"hello".to_vec()),
                (Some(2_500_000), Vec::new())
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_cue_older_than_the_presentation_starts_with_it() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&captioned_catalog());
        // The last caption sent before this origin subscribed is still on
        // screen, but predates every frame of audio and video it will receive.
        fixture.publish_legacy_frame("captions", 100_000, b"still showing");
        fixture.publish_frame("1080p", 4_000_000, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 4_020_000, &[0xFC]);
        let discovery = source.discover(discovery_limits()).await?;
        fixture.publish_legacy_frame("captions", 4_500_000, b"next");
        fixture.finish_media();
        fixture.producer.close();

        let mut packets = Vec::new();
        while source.fill(&mut packets).await? == InputState::Open {}
        assert_eq!(
            cues(&discovery, &packets),
            vec![
                (Some(4_000_000), b"still showing".to_vec()),
                (Some(4_500_000), b"next".to_vec())
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn finishing_every_track_is_a_closed_input() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);
        source.discover(discovery_limits()).await?;
        fixture.finish_media();
        fixture.producer.close();

        let mut packets = Vec::new();
        let mut state = InputState::Open;
        while state == InputState::Open {
            state = source.fill(&mut packets).await?;
        }
        assert_eq!(state, InputState::Closed);
        Ok(())
    }
    #[tokio::test]
    async fn discovery_deadline_includes_waiting_for_a_broadcast() -> Result<(), SourceError> {
        let (origin, _driver) = moq_net::origin::Producer::new(moq_net::origin::Config::default());
        let meters = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let mut source = MoqPacketSource::from_origin(
            &origin.consume(),
            None,
            moq_net::cache::Pool::unbounded(),
            InputLimits::permissive(),
            meters.source_view(),
        )?;
        let limits = DiscoveryLimits {
            maximum_wall_time: std::time::Duration::from_millis(10),
            ..discovery_limits()
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), source.discover(limits)).await;
        assert!(matches!(
            result,
            Ok(Err(SourceError::Discovery(
                DiscoveryProblem::DeadlineExceeded
            )))
        ));
        Ok(())
    }

    #[test]
    fn the_receive_cache_is_charged_to_the_publisher_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::domain::{PipelineBudget, Stage};

        let pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default().with_capacity(64 * 1024 * 1024),
        );
        let mut info = moq_net::broadcast::Info::new();
        info.pool = pool.clone();
        let broadcast = info.produce();
        let track = broadcast.create_track("video", None)?;
        let mut group = track.append_group()?;
        group.write_frame(moq_net::Timestamp::ZERO, vec![0_u8; 1024 * 1024])?;
        let cached = usize::try_from(pool.used())?;
        assert!(
            cached >= 1024 * 1024,
            "the dependency counts the frame it holds"
        );

        let budget = PipelineBudget::new(8 * 1024 * 1024);
        let mut charge = CacheCharge {
            pool: pool.clone(),
            held: None,
        };
        charge.reconcile(Some(&budget))?;
        assert_eq!(budget.used(), cached);
        charge.reconcile(Some(&budget))?;
        assert_eq!(
            budget.used(),
            cached,
            "an unchanged cache is not charged twice"
        );

        let small = PipelineBudget::new(512 * 1024);
        let error = CacheCharge { pool, held: None }
            .reconcile(Some(&small))
            .expect_err("a cache larger than the budget refuses the publisher");
        assert_eq!(error.stage, Stage::MoqCache);

        drop(charge);
        assert_eq!(budget.used(), 0, "the charge is released with the source");
        Ok(())
    }

    #[tokio::test]
    async fn fill_returns_ready_packets_while_tracks_are_live() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);
        source.discover(discovery_limits()).await?;
        source.limits.maximum_packets_per_batch = 1;
        for _ in 0..2 {
            let mut packets = Vec::new();
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(1), source.fill(&mut packets))
                    .await;
            assert_eq!(
                result.expect("a full batch does not wait for another frame")?,
                InputState::Open
            );
            assert_eq!(packets.len(), 1);
        }
        fixture.publish_frame("opus", 20_000, &[0xFC]);
        source.limits.maximum_packets_per_batch = 100;
        let mut packets = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), source.fill(&mut packets))
            .await
            .expect("partial batches return when no media is ready")?;
        assert_eq!(packets.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn discovery_accounts_for_catalog_bytes() {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        let limits = DiscoveryLimits {
            maximum_probe_bytes: 1,
            ..discovery_limits()
        };
        assert!(matches!(
            source.discover(limits).await,
            Err(SourceError::Discovery(DiscoveryProblem::ProbeLimitExceeded))
        ));
    }

    #[tokio::test]
    async fn ready_video_cannot_starve_audio_discovery() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);
        source.ensure_catalog_subscription().await?;
        let Incoming::Catalog(Some(catalog)) = source.next_incoming().await? else {
            panic!("catalog");
        };
        let mapped = map::tracks_from_catalog(
            &catalog.video.renditions,
            &catalog.audio.renditions,
            &catalog.text.renditions,
        )?;
        source.subscribe_tracks(&mapped).await?;
        for sequence in 1..5 {
            fixture.publish_frame("1080p", sequence * 33_333, crate::mux::fixtures::H264_IDR);
        }
        assert!(matches!(
            source.next_incoming().await?,
            Incoming::Frame { index: 0, .. }
        ));
        assert!(matches!(
            source.next_incoming().await?,
            Incoming::Frame { index: 1, .. }
        ));
        Ok(())
    }
    #[tokio::test]
    async fn delayed_opus_head_updates_prefetched_audio_timing() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        let mut catalog = loc_catalog();
        fixture.publish_catalog(&catalog);
        fixture.track("1080p");
        fixture.publish_frame("opus", 1_000_000, &[0xfc]);
        let mut discovery = source.discover(discovery_limits());
        // Drive discovery through the provisional catalog and audio, leaving
        // video pending so the real encoder header arrives before freeze.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut discovery)
                .await
                .is_err()
        );
        catalog["audio"]["renditions"]["opus"]["description"] =
            super::super::catalog::encode_hex(&super::super::fixtures::opus_head(312)).into();
        fixture.publish_catalog(&catalog);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut discovery)
                .await
                .is_err()
        );
        fixture.publish_frame("1080p", 1_000_000, crate::mux::fixtures::H264_IDR);
        let report = discovery.await?;
        let audio = report.tracks.get(crate::domain::TrackId(1)).expect("audio");
        assert_eq!(audio.first_pts, Some(1_006_500));
        let mut packets = Vec::new();
        source.fill(&mut packets).await?;
        let audio = packets
            .iter()
            .find(|packet| packet.track_id == crate::domain::TrackId(1))
            .expect("audio");
        assert_eq!(audio.pts, Some(1_000_000));
        assert_eq!(audio.duration, Some(20_000));
        Ok(())
    }
    #[tokio::test]
    async fn disconnecting_the_catalog_interrupts_the_publication() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xfc]);
        source.discover(discovery_limits()).await?;
        drop(fixture);
        let mut packets = Vec::new();
        assert_eq!(source.fill(&mut packets).await?, InputState::Interrupted);
        assert_eq!(packets.len(), 2);
        Ok(())
    }
}
