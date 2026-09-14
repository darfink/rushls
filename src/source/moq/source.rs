//! Streaming Hang media behind a bounded `PacketSource`.
//!
//! Discovery fixes the track set, accepts provisional Opus configuration, and
//! waits until each track has a presentation timestamp. After discovery, an
//! added or removed track or a codec-config change is fatal. Media is read in
//! group order and never silently skipped: a sequence gap fails the publication.

use std::{collections::VecDeque, sync::Arc, task::Poll, time::Instant};

use crate::{
    domain::{Appender, BoxFuture, DiscoveredTrack, TrackCatalog, TrackId},
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
    announced: Option<moq_net::announce::Consumer>,
    broadcast_path: Option<String>,
    /// Held so the transport stays open for as long as this source does.
    _session: Option<moq_net::Session>,
    prefetch: VecDeque<Packet>,
    terminal: Option<InputState>,
    next_track: usize,
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
        limits: InputLimits,
        meters: Arc<dyn SourceMeters>,
    ) -> Result<Self, SourceError> {
        limits.validate()?;
        Ok(Self {
            announced: Some(origin.announced()),
            _session: session,
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
            announced: None,
            broadcast_path: None,
            _session: None,
            prefetch: VecDeque::new(),
            terminal: None,
            next_track: 0,
        }
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
            .subscribe(Some(catalog::Reader::subscription()))
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
        loop {
            let Some(announce) = announced.next().await else {
                return Err(DiscoveryProblem::Abandoned.into());
            };
            let Some(broadcast) = announce.broadcast else {
                continue;
            };
            self.broadcast_path = Some(announce.path.to_string());
            return Ok(broadcast);
        }
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
                .subscribe(Some(
                    moq_net::track::Subscription::default()
                        .with_ordered(true)
                        .with_latency_max(std::time::Duration::from_secs(30)),
                ))
                .await
                .map_err(classify_net)?;
            self.tracks.push(LiveTrack {
                id: track.id,
                codec: track.codec,
                reader: loc::Reader::new(subscriber, mapped.legacy[index]),
                ended: false,
                inline: mapped.inline_h264[index].then(super::h264::Inline::default),
            });
        }
        Ok(())
    }

    fn media_packet(&mut self, index: usize, mut frame: loc::Frame) -> Result<Packet, SourceError> {
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
        map::packet(
            track.id,
            track.codec,
            &frame,
            self.limits.maximum_payload_bytes_per_packet,
        )
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

    fn store_discovery(&mut self, mapped: MappedCatalog) -> DiscoveryReport {
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
                        let packet = map::packet(
                            track.id,
                            track.codec,
                            &frame,
                            self.limits.maximum_payload_bytes_per_packet,
                        )?;
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
                        path: announce.path.to_string(),
                        present: announce.broadcast.is_some(),
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
        .all(|track| track.first_pts.is_some() && !track.codec_extradata.is_empty())
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

    use super::super::fixtures::{Fixture, discovery_limits, loc_catalog};

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

    #[tokio::test]
    async fn finishing_every_track_is_a_closed_input() -> Result<(), SourceError> {
        let (mut fixture, mut source) = Fixture::new();
        fixture.publish_catalog(&loc_catalog());
        fixture.publish_frame("1080p", 0, crate::mux::fixtures::H264_IDR);
        fixture.publish_frame("opus", 0, &[0xFC]);
        source.discover(discovery_limits()).await?;
        fixture.finish_media();
        fixture.producer.finish();

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
        let origin = moq_net::Origin::random().produce();
        let meters = crate::observe::SessionMeters::new(crate::observe::ProcessMeters::default());
        let mut source = MoqPacketSource::from_origin(
            &origin.consume(),
            None,
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
        let mapped =
            map::tracks_from_catalog(&catalog.video.renditions, &catalog.audio.renditions)?;
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
