use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use parking_lot::RwLock;
use thiserror::Error;
use tokio::{sync::watch, time::Instant};

use crate::{
    domain::{Payload, RenditionId, StreamId, TickDuration, TickTimestamp},
    mux::{InitializationSegment, MuxedMedia},
};

/// How much of a stream stays fetchable behind the live edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryWindow {
    pub segments: usize,
    pub parts: usize,
}

impl Default for DeliveryWindow {
    fn default() -> Self {
        Self {
            segments: 6,
            parts: 24,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreLimits {
    /// Retention window, fixed per store rather than per publisher.
    ///
    /// A stream outlives the session that created it, so letting each publisher
    /// choose would mean a reconnecting encoder could silently change how much
    /// history its viewers can reach.
    pub window: DeliveryWindow,
    pub maximum_streams: usize,
    /// How long a stream stays fetchable after its publisher leaves.
    ///
    /// This is the reconnect budget. Viewers keep their playlist position
    /// across a brief publisher outage, and a stream is only forgotten once
    /// nobody has written to it for this long.
    pub idle_retention: Duration,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            window: DeliveryWindow::default(),
            maximum_streams: 1_024,
            idle_retention: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the delivery store is already holding its maximum of {maximum} streams")]
pub struct StoreFull {
    pub maximum: usize,
}

/// A closed segment held for delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSegment {
    /// Which publication produced this. See [`StoredPart::publication`].
    pub publication: u64,
    /// Which header decodes this. See [`StoredPart::initialization`].
    pub initialization: u64,
    pub media_sequence: u64,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub payload: Payload,
}

/// A partial segment held for delivery, addressed within its parent segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPart {
    /// Which publication produced this.
    ///
    /// Media from two publications cannot be assumed to share a timeline, so a
    /// playlist writer emits a discontinuity wherever this changes between
    /// neighbouring segments. Recording it is enough; nothing here has to
    /// decide what a discontinuity means.
    pub publication: u64,
    /// Which [`StoredInitialization`] this was muxed against.
    ///
    /// Tracked separately from `publication` because the two answer different
    /// questions and do not always change together. A publisher reconnecting
    /// with identical encoder settings produces a new timeline but the same
    /// header, and a publisher that changes resolution mid-publication produces
    /// a new header on the same timeline. A playlist writer needs a
    /// discontinuity for the first and an `EXT-X-MAP` for the second.
    pub initialization: u64,
    pub media_sequence: u64,
    pub part_index: u64,
    pub independent: bool,
    pub media_start: TickTimestamp,
    pub duration: TickDuration,
    pub payload: Payload,
}

/// An initialization segment, identified so stored media can point at it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredInitialization {
    /// Unique within this rendition for the life of the stream.
    pub id: u64,
    pub segment: InitializationSegment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenditionSnapshot {
    pub rendition_id: RenditionId,
    /// Every header some retained object still needs.
    ///
    /// Not a single current one. A new publisher re-runs discovery,
    /// calibration, and pre-roll and builds a fresh muxer, so its
    /// initialization segment is generally different — while segments muxed
    /// against the *previous* one are still inside the delivery window and
    /// still fetchable. Keeping only the newest would leave those segments
    /// undecodable, which is precisely what `EXT-X-MAP` exists to prevent.
    pub initializations: Vec<StoredInitialization>,
    pub segments: Vec<StoredSegment>,
    pub parts: Vec<StoredPart>,
}

impl RenditionSnapshot {
    /// The header a stored segment or part must be decoded with.
    ///
    /// This is the `EXT-X-MAP` a playlist writer emits ahead of that object,
    /// looked up by its [`StoredSegment::initialization`].
    pub fn initialization_for(&self, initialization: u64) -> Option<&InitializationSegment> {
        self.initializations
            .iter()
            .find(|held| held.id == initialization)
            .map(|held| &held.segment)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamSnapshot {
    pub revision: u64,
    pub ended: bool,
    /// True while no publisher holds the write lease.
    pub idle: bool,
    pub renditions: Vec<RenditionSnapshot>,
}

/// The process-wide set of streams available to viewers.
///
/// Cloning shares one store, so it can be handed to every ingest session and to
/// the HTTP layer at startup. Sessions write into it as parts close and readers
/// snapshot out of it, which means serving never reaches into session state and
/// a session ending does not interrupt a request already in flight.
///
/// A stream's lifetime is deliberately longer than any one publisher's. See
/// [`Self::lease`].
#[derive(Clone, Debug)]
pub struct StreamStore {
    streams: Arc<RwLock<HashMap<StreamId, Arc<LiveStream>>>>,
    limits: StoreLimits,
}

impl Default for StreamStore {
    fn default() -> Self {
        Self::new(StoreLimits::default())
    }
}

impl StreamStore {
    pub fn new(limits: StoreLimits) -> Self {
        Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
            limits,
        }
    }

    pub fn limits(&self) -> StoreLimits {
        self.limits
    }

    /// Takes the write lease on a stream, creating it if nobody has yet.
    ///
    /// The stream itself is *not* owned by the lease. That separation is what
    /// makes takeover survivable: a replacement publisher inherits the existing
    /// [`LiveStream`], so media sequence numbering continues, the retained
    /// window stays fetchable, and viewers see no window in which the stream
    /// does not exist. Compare removing the stream when its publisher leaves,
    /// which resets sequence numbers — something HLS forbids — and blanks the
    /// stream for the length of the handover.
    ///
    /// Leasing revokes any outstanding lease. The displaced holder can still
    /// call [`StreamLease::write`], but its writes are dropped rather than
    /// interleaved into its successor's media.
    pub fn lease(&self, stream: StreamId) -> Result<StreamLease, StoreFull> {
        let mut streams = self.streams.write();
        let live = match streams.get(&stream) {
            Some(live) => Arc::clone(live),
            None => {
                if streams.len() >= self.limits.maximum_streams {
                    return Err(StoreFull {
                        maximum: self.limits.maximum_streams,
                    });
                }
                let live = Arc::new(LiveStream::new(self.limits.window));
                streams.insert(stream.clone(), Arc::clone(&live));
                live
            }
        };

        // Attached while the map is still locked. A resumed stream is idle
        // right up to this call, so releasing the map first would leave a
        // window in which a concurrent `retire_idle` could forget it — and this
        // lease would then publish into a `LiveStream` no reader can reach.
        // Locks are only ever taken store-then-stream, so nesting is safe.
        let publication = live.attach();
        drop(streams);

        Ok(StreamLease {
            stream,
            live,
            publication,
        })
    }

    pub fn get(&self, stream: &StreamId) -> Option<Arc<LiveStream>> {
        self.streams.read().get(stream).map(Arc::clone)
    }

    pub fn streams(&self) -> Vec<StreamId> {
        self.streams.read().keys().cloned().collect()
    }

    /// Streams held, whether or not anyone is currently publishing them.
    pub fn len(&self) -> usize {
        self.streams.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.read().is_empty()
    }

    /// Streams with a publisher attached right now.
    ///
    /// Reported separately from [`Self::len`] because the gap between them is
    /// exactly the set of streams waiting for an encoder to come back, which is
    /// what an operator watching a flaky contribution link wants to see.
    pub fn leased(&self) -> usize {
        self.streams
            .read()
            .values()
            .filter(|live| !live.is_idle())
            .count()
    }

    /// Forgets streams nobody has written to for [`StoreLimits::idle_retention`].
    ///
    /// Called on a timer by the process that owns the store. Sweeping is
    /// explicit rather than automatic on lease release because the whole point
    /// of retaining an unleased stream is that a publisher may come back for
    /// it.
    ///
    /// Returns how many streams were forgotten.
    pub fn retire_idle(&self) -> usize {
        let cutoff = self.limits.idle_retention;
        let mut streams = self.streams.write();
        let before = streams.len();
        streams.retain(|_, live| {
            if !live.is_idle_for(cutoff) {
                return true;
            }
            // Expiry of the reconnect budget is the moment an absent publisher
            // becomes a finished stream. Saying so wakes readers parked in
            // `wait_after`, which would otherwise hold an `Arc` to a stream
            // nothing can ever bump again.
            live.retire();
            false
        });
        before - streams.len()
    }
}

/// A publisher's write lease on one stream.
///
/// Dropping it leaves the stream in place and idle, ready to be resumed. Ending
/// a stream is a separate, deliberate act — see [`Self::end`] — because "my
/// publisher went away" and "this stream is over" are different facts and only
/// the second one should stop viewers waiting for more media.
#[derive(Debug)]
pub struct StreamLease {
    stream: StreamId,
    live: Arc<LiveStream>,
    publication: u64,
}

impl StreamLease {
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    pub fn live(&self) -> &Arc<LiveStream> {
        &self.live
    }

    /// Which publication this lease writes as.
    pub fn publication(&self) -> u64 {
        self.publication
    }

    /// True once another publisher has taken the stream over.
    pub fn is_revoked(&self) -> bool {
        self.live.current_publication() != self.publication
    }

    /// Publishes one object, returning false if this lease has been revoked.
    ///
    /// A revoked lease's media is discarded rather than appended. An incumbent
    /// draining its pipeline after being replaced must not interleave a stale
    /// tail into the media its successor is already publishing.
    pub fn write(&self, media: MuxedMedia) -> bool {
        self.live.write(self.publication, media)
    }

    /// Marks the stream complete so readers stop waiting for new media.
    ///
    /// A no-op from a revoked lease: a stream taken over by a new publisher is
    /// emphatically not over.
    pub fn end(&self) -> bool {
        self.live.end(self.publication)
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.live.release(self.publication);
    }
}

/// Live media for one stream, shared by its publishers and every reader.
#[derive(Debug)]
pub struct LiveStream {
    window: DeliveryWindow,
    state: RwLock<StreamState>,
    revision: watch::Sender<u64>,
}

#[derive(Debug)]
struct StreamState {
    renditions: Vec<RenditionState>,
    ended: bool,
    /// Counter of publications this stream has had, and the current lease's id.
    publication: u64,
    /// When the current lease was released, if there is no lease.
    idle_since: Option<Instant>,
}

#[derive(Debug)]
struct RenditionState {
    rendition_id: RenditionId,
    initializations: Vec<StoredInitialization>,
    /// Header the next object written will be decoded with.
    ///
    /// Zero until a muxer emits one, which no stored object can then resolve —
    /// correctly, since media that arrived before its header is unplayable.
    current_initialization: u64,
    /// Ids handed out so far, so a replaced header never reuses one.
    issued_initializations: u64,
    segments: VecDeque<StoredSegment>,
    parts: VecDeque<StoredPart>,
    /// Sequence number the currently accumulating segment will be given.
    ///
    /// Lives on the stream, not the lease, so it survives a change of
    /// publisher. HLS requires media sequence numbers to increase for the life
    /// of the playlist, and a client that saw sequence 400 will not accept a
    /// reconnecting encoder restarting at 0.
    next_media_sequence: u64,
    /// Index of the next part within that segment.
    next_part_index: u64,
}

impl LiveStream {
    fn new(window: DeliveryWindow) -> Self {
        Self {
            window,
            state: RwLock::new(StreamState {
                renditions: Vec::new(),
                ended: false,
                publication: 0,
                idle_since: Some(Instant::now()),
            }),
            revision: watch::Sender::new(0),
        }
    }

    pub fn revision(&self) -> u64 {
        *self.revision.borrow()
    }

    pub fn is_ended(&self) -> bool {
        self.state.read().ended
    }

    pub fn is_idle(&self) -> bool {
        self.state.read().idle_since.is_some()
    }

    pub fn current_publication(&self) -> u64 {
        self.state.read().publication
    }

    pub fn snapshot(&self) -> StreamSnapshot {
        let state = self.state.read();
        StreamSnapshot {
            revision: self.revision(),
            ended: state.ended,
            idle: state.idle_since.is_some(),
            renditions: state
                .renditions
                .iter()
                .map(RenditionState::snapshot)
                .collect(),
        }
    }

    pub fn rendition(&self, rendition_id: RenditionId) -> Option<RenditionSnapshot> {
        self.state
            .read()
            .renditions
            .iter()
            .find(|rendition| rendition.rendition_id == rendition_id)
            .map(RenditionState::snapshot)
    }

    /// Resolves once the stream advances past `revision`, or once it ends.
    ///
    /// This is the primitive an LL-HLS blocking playlist reload is built on: a
    /// request holding the client's current revision parks here instead of
    /// polling, and wakes on the part that satisfies it.
    pub async fn wait_after(&self, revision: u64) -> u64 {
        let mut updates = self.revision.subscribe();
        loop {
            let current = *updates.borrow_and_update();
            if current > revision || self.is_ended() {
                return current;
            }
            if updates.changed().await.is_err() {
                return current;
            }
        }
    }

    fn is_idle_for(&self, duration: Duration) -> bool {
        self.state
            .read()
            .idle_since
            .is_some_and(|since| Instant::now().saturating_duration_since(since) >= duration)
    }

    /// Starts a new publication, returning its id.
    fn attach(&self) -> u64 {
        let mut state = self.state.write();
        state.publication += 1;
        state.ended = false;
        state.idle_since = None;
        for rendition in &mut state.renditions {
            rendition.abandon_open_segment();
            rendition.forget_unreachable_initializations();
        }
        let publication = state.publication;
        drop(state);

        self.bump();
        publication
    }

    /// Marks a stream finished because nobody came back for it.
    fn retire(&self) {
        let mut state = self.state.write();
        if state.ended {
            return;
        }
        state.ended = true;
        drop(state);

        self.bump();
    }

    fn release(&self, publication: u64) {
        let mut state = self.state.write();
        if state.publication != publication || state.idle_since.is_some() {
            return;
        }
        state.idle_since = Some(Instant::now());
        drop(state);

        // Wake blocked readers so they re-evaluate. They will find no new media
        // and park again, which is correct: the stream may yet be resumed.
        self.bump();
    }

    fn write(&self, publication: u64, media: MuxedMedia) -> bool {
        let mut state = self.state.write();
        if state.publication != publication {
            return false;
        }

        let window = self.window;
        let rendition = state.rendition_mut(media.rendition_id());
        match media {
            MuxedMedia::Initialization(segment) => {
                rendition.set_initialization(segment);
            }
            MuxedMedia::Part(part) => {
                rendition.parts.push_back(StoredPart {
                    publication,
                    initialization: rendition.current_initialization,
                    media_sequence: rendition.next_media_sequence,
                    part_index: rendition.next_part_index,
                    independent: part.independent,
                    media_start: part.media_start,
                    duration: part.duration,
                    payload: part.payload,
                });
                rendition.next_part_index += 1;
                while rendition.parts.len() > window.parts {
                    rendition.parts.pop_front();
                }
            }
            MuxedMedia::Segment(segment) => {
                rendition.segments.push_back(StoredSegment {
                    publication,
                    initialization: rendition.current_initialization,
                    media_sequence: rendition.next_media_sequence,
                    media_start: segment.media_start,
                    duration: segment.duration,
                    payload: segment.payload,
                });
                rendition.next_media_sequence += 1;
                rendition.next_part_index = 0;
                while rendition.segments.len() > window.segments {
                    rendition.segments.pop_front();
                }
            }
        }
        rendition.forget_unreachable_initializations();
        drop(state);

        // Waking readers after the lock is released keeps a blocked playlist
        // request from contending with the publisher that just satisfied it.
        self.bump();
        true
    }

    fn end(&self, publication: u64) -> bool {
        let mut state = self.state.write();
        if state.publication != publication || state.ended {
            return false;
        }
        state.ended = true;
        drop(state);

        self.bump();
        true
    }

    fn bump(&self) {
        self.revision.send_modify(|revision| *revision += 1);
    }
}

impl StreamState {
    fn rendition_mut(&mut self, rendition_id: RenditionId) -> &mut RenditionState {
        if let Some(index) = self
            .renditions
            .iter()
            .position(|rendition| rendition.rendition_id == rendition_id)
        {
            return &mut self.renditions[index];
        }

        self.renditions.push(RenditionState {
            rendition_id,
            initializations: Vec::new(),
            current_initialization: 0,
            issued_initializations: 0,
            segments: VecDeque::new(),
            parts: VecDeque::new(),
            next_media_sequence: 0,
            next_part_index: 0,
        });
        self.renditions
            .last_mut()
            .expect("a rendition was just pushed")
    }
}

impl RenditionState {
    fn snapshot(&self) -> RenditionSnapshot {
        RenditionSnapshot {
            rendition_id: self.rendition_id,
            initializations: self.initializations.clone(),
            segments: self.segments.iter().cloned().collect(),
            parts: self.parts.iter().cloned().collect(),
        }
    }

    /// Records the header subsequent media will be muxed against.
    ///
    /// Re-issuing the same bytes is free — a muxer may emit its header ahead of
    /// every segment — and only different bytes start a new one. Overwriting in
    /// place instead would silently invalidate media already retained under the
    /// old header, which is the same bug at a finer grain than losing a
    /// predecessor's header on takeover.
    fn set_initialization(&mut self, segment: InitializationSegment) {
        let unchanged = self
            .initializations
            .iter()
            .any(|held| held.id == self.current_initialization && held.segment == segment);
        if unchanged {
            return;
        }

        self.issued_initializations += 1;
        self.current_initialization = self.issued_initializations;
        self.initializations.push(StoredInitialization {
            id: self.current_initialization,
            segment,
        });
    }

    /// Drops headers no retained object can still be decoded with.
    ///
    /// Without this a stream that churned through publishers would accumulate a
    /// header per publication for the life of the process. The current one is
    /// always kept: no media has been written under it yet.
    fn forget_unreachable_initializations(&mut self) {
        if self.initializations.len() < 2 {
            return;
        }
        let segments = &self.segments;
        let parts = &self.parts;
        let current = self.current_initialization;
        self.initializations.retain(|held| {
            held.id == current
                || segments
                    .iter()
                    .any(|segment| segment.initialization == held.id)
                || parts.iter().any(|part| part.initialization == held.id)
        });
    }

    /// Drops the parts of a segment the departing publisher never closed.
    ///
    /// A segment is the smallest thing HLS can mark discontinuous or hang an
    /// `EXT-X-MAP` on, so one cannot straddle two publishers: parts from
    /// unrelated timestamp epochs inside a single `EXT-INF` are undecodable and
    /// there is no tag that says otherwise. The incumbent will never close this
    /// segment, so nothing can ever cover these parts.
    ///
    /// The sequence number is reused by the successor rather than skipped.
    /// Segments are numbered consecutively from `EXT-X-MEDIA-SEQUENCE`, so a
    /// gap does not mean "one is missing" — it misaddresses every segment after
    /// it.
    fn abandon_open_segment(&mut self) {
        if self.next_part_index == 0 {
            return;
        }
        let open = self.next_media_sequence;
        self.parts.retain(|part| part.media_sequence != open);
        self.next_part_index = 0;
    }
}

#[cfg(test)]
mod tests {
    use crate::mux::{ContainerFormat, MuxedPart, MuxedSegment};

    use super::*;

    fn stream() -> StreamId {
        StreamId::new("live/camera")
    }

    fn store() -> StreamStore {
        StreamStore::new(StoreLimits {
            window: DeliveryWindow {
                segments: 2,
                parts: 3,
            },
            maximum_streams: 2,
            idle_retention: Duration::from_secs(30),
        })
    }

    fn part(media_start: TickTimestamp, independent: bool) -> MuxedMedia {
        MuxedMedia::Part(MuxedPart {
            rendition_id: RenditionId(0),
            media_start,
            duration: 18_000,
            independent,
            payload: Payload::from(vec![1, 2, 3]),
        })
    }

    fn segment(media_start: TickTimestamp) -> MuxedMedia {
        MuxedMedia::Segment(MuxedSegment {
            rendition_id: RenditionId(0),
            media_start,
            duration: 180_000,
            payload: Payload::from(vec![4, 5, 6]),
        })
    }

    fn rendition(lease: &StreamLease) -> RenditionSnapshot {
        lease
            .live()
            .rendition(RenditionId(0))
            .expect("rendition exists")
    }

    #[tokio::test(start_paused = true)]
    async fn parts_are_numbered_within_the_segment_they_belong_to() {
        let lease = store().lease(stream()).expect("the store has room");

        lease.write(part(0, true));
        lease.write(part(18_000, false));
        lease.write(segment(0));
        lease.write(part(180_000, true));

        let rendition = rendition(&lease);
        let numbering: Vec<_> = rendition
            .parts
            .iter()
            .map(|part| (part.media_sequence, part.part_index))
            .collect();
        assert_eq!(numbering, vec![(0, 0), (0, 1), (1, 0)]);
        assert_eq!(rendition.segments.len(), 1);
        assert_eq!(rendition.segments[0].media_sequence, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_delivery_window_bounds_retention() {
        let lease = store().lease(stream()).expect("the store has room");

        for index in 0..5 {
            lease.write(part(index * 18_000, index == 0));
            lease.write(segment(index * 180_000));
        }

        let rendition = rendition(&lease);
        assert_eq!(rendition.segments.len(), 2);
        assert_eq!(rendition.parts.len(), 3);
        assert_eq!(rendition.segments[0].media_sequence, 3);
    }

    fn initialization(version: u64) -> MuxedMedia {
        MuxedMedia::Initialization(InitializationSegment {
            rendition_id: RenditionId(0),
            format: ContainerFormat::Cmaf,
            version,
            payload: Payload::from(vec![version as u8]),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_header_changing_mid_publication_leaves_the_old_one_resolvable() {
        let lease = store().lease(stream()).expect("the store has room");

        lease.write(initialization(1));
        lease.write(segment(0));
        lease.write(initialization(2));
        lease.write(segment(180_000));

        // Replacing in place would have left the first segment — still inside
        // the window, still fetchable — pointing at a header that no longer
        // decodes it.
        let snapshot = rendition(&lease);
        assert_eq!(
            snapshot
                .segments
                .iter()
                .map(|held| snapshot
                    .initialization_for(held.initialization)
                    .map(|header| header.version))
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn re_emitting_an_unchanged_header_does_not_start_a_new_one() {
        let lease = store().lease(stream()).expect("the store has room");

        lease.write(initialization(1));
        lease.write(segment(0));
        lease.write(initialization(1));
        lease.write(segment(180_000));

        let snapshot = rendition(&lease);
        assert_eq!(snapshot.initializations.len(), 1);
        assert_eq!(
            snapshot
                .segments
                .iter()
                .map(|held| held.initialization)
                .collect::<Vec<_>>(),
            vec![1, 1]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_takeover_keeps_the_header_its_predecessors_media_still_needs() {
        let store = store();
        let first = store.lease(stream()).expect("the store has room");
        first.write(initialization(1));
        first.write(segment(0));
        drop(first);

        let second = store.lease(stream()).expect("the stream is resumable");
        second.write(initialization(2));
        second.write(segment(0));

        // A new publisher re-runs discovery, calibration, and pre-roll, so its
        // header is generally different — and the predecessor's segment is
        // still inside the window and still fetchable.
        let snapshot = rendition(&second);
        assert_eq!(snapshot.segments.len(), 2);
        assert_eq!(
            snapshot
                .segments
                .iter()
                .map(|held| snapshot
                    .initialization_for(held.initialization)
                    .map(|header| header.version))
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2)],
            "each retained segment resolves to the header it was muxed against"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnecting_publisher_with_unchanged_settings_reuses_its_header() {
        let store = store();
        let first = store.lease(stream()).expect("the store has room");
        first.write(initialization(1));
        first.write(segment(0));
        drop(first);

        let second = store.lease(stream()).expect("the stream is resumable");
        second.write(initialization(1));
        second.write(segment(0));

        let snapshot = rendition(&second);
        assert_eq!(
            snapshot.initializations.len(),
            1,
            "identical bytes decode both publications"
        );
        assert_eq!(
            snapshot
                .segments
                .iter()
                .map(|held| (held.publication, held.initialization))
                .collect::<Vec<_>>(),
            vec![(1, 1), (2, 1)],
            "the timeline still restarts, so the discontinuity is recorded anyway"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn headers_are_forgotten_once_their_media_leaves_the_window() {
        let store = store();
        let first = store.lease(stream()).expect("the store has room");
        first.write(initialization(1));
        first.write(segment(0));
        drop(first);

        let second = store.lease(stream()).expect("the stream is resumable");
        second.write(initialization(2));
        // The window holds two segments, so these push the predecessor's out.
        second.write(segment(0));
        second.write(segment(180_000));
        second.write(segment(360_000));

        let headers = rendition(&second).initializations;
        assert_eq!(
            headers.iter().map(|held| held.id).collect::<Vec<_>>(),
            vec![2],
            "a header nothing retained can be decoded with is dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_takeover_abandons_the_segment_the_incumbent_left_open() {
        let store = store();
        let incumbent = store.lease(stream()).expect("the store has room");
        incumbent.write(initialization(1));
        incumbent.write(segment(0));
        incumbent.write(part(180_000, true));
        incumbent.write(part(198_000, false));

        let successor = store.lease(stream()).expect("a takeover always fits");
        successor.write(initialization(2));
        successor.write(part(0, true));
        successor.write(segment(0));

        let snapshot = rendition(&successor);
        assert_eq!(
            snapshot
                .parts
                .iter()
                .map(|held| (held.publication, held.media_sequence, held.part_index))
                .collect::<Vec<_>>(),
            vec![(2, 1, 0)],
            "the incumbent's uncoverable parts are dropped rather than mixed \
             into a segment the successor closes"
        );
        assert_eq!(
            snapshot
                .segments
                .iter()
                .map(|held| (held.publication, held.media_sequence))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 1)],
            "the abandoned sequence number is reused, not skipped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_waiting_out_a_publisher_outage_wakes_when_it_expires() {
        let store = store();
        drop(store.lease(stream()).expect("the store has room"));
        let live = store.get(&stream()).expect("retained for reconnect");
        let start = live.revision();

        let reader = {
            let live = Arc::clone(&live);
            tokio::spawn(async move { live.wait_after(start).await })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(store.retire_idle(), 1);

        tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("a forgotten stream must not strand its readers")
            .expect("the reader task succeeds");
        assert!(live.is_ended());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_outlives_the_publisher_that_created_it() {
        let store = store();
        let lease = store.lease(stream()).expect("the store has room");
        lease.write(segment(0));

        drop(lease);

        let live = store.get(&stream()).expect("the stream is still fetchable");
        assert!(live.is_idle());
        assert!(
            !live.is_ended(),
            "an absent publisher is not an ended stream"
        );
        assert_eq!(live.rendition(RenditionId(0)).unwrap().segments.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resumed_stream_continues_its_media_sequence() {
        let store = store();
        let first = store.lease(stream()).expect("the store has room");
        first.write(segment(0));
        first.write(segment(180_000));
        drop(first);

        let second = store.lease(stream()).expect("the stream is resumable");
        second.write(segment(0));

        let segments = rendition(&second).segments;
        assert_eq!(
            segments
                .iter()
                .map(|s| s.media_sequence)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "numbering continues across publishers rather than restarting"
        );
        assert_eq!(
            segments.iter().map(|s| s.publication).collect::<Vec<_>>(),
            vec![1, 2],
            "the publication change is recorded so a playlist can mark it discontinuous"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_takeover_revokes_the_incumbents_lease_without_an_outage() {
        let store = store();
        let incumbent = store.lease(stream()).expect("the store has room");
        incumbent.write(segment(0));

        let successor = store.lease(stream()).expect("a takeover always fits");
        assert!(incumbent.is_revoked());
        assert!(!successor.is_revoked());

        assert!(
            !incumbent.write(segment(180_000)),
            "a revoked lease's drain must not interleave into its successor"
        );
        assert!(!incumbent.end(), "a taken-over stream is not over");
        assert!(!store.get(&stream()).expect("still published").is_ended());

        drop(incumbent);
        let live = store.get(&stream()).expect("the successor still holds it");
        assert!(
            !live.is_idle(),
            "the incumbent's drop must not idle its successor's stream"
        );
        assert_eq!(live.rendition(RenditionId(0)).unwrap().segments.len(), 1);
        drop(successor);
    }

    #[tokio::test(start_paused = true)]
    async fn ending_a_stream_is_deliberate_and_stops_readers_waiting() {
        let store = store();
        let lease = store.lease(stream()).expect("the store has room");

        assert!(lease.end());
        assert!(store.get(&stream()).expect("still present").is_ended());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_streams_are_forgotten_only_after_their_reconnect_budget() {
        let store = store();
        drop(store.lease(stream()).expect("the store has room"));

        tokio::time::advance(Duration::from_secs(29)).await;
        assert_eq!(store.retire_idle(), 0);
        assert_eq!(store.len(), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(store.retire_idle(), 1);
        assert!(store.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_leased_stream_is_never_retired() {
        let store = store();
        let lease = store.lease(stream()).expect("the store has room");

        tokio::time::advance(Duration::from_secs(600)).await;

        assert_eq!(store.retire_idle(), 0);
        assert_eq!(store.len(), 1);
        drop(lease);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_store_refuses_new_streams_but_still_allows_takeover() {
        let store = store();
        let first = store.lease(StreamId::new("a")).expect("room for the first");
        let _second = store
            .lease(StreamId::new("b"))
            .expect("room for the second");

        assert_eq!(
            store.lease(StreamId::new("c")).map(|_| ()),
            Err(StoreFull { maximum: 2 })
        );
        drop(first);
        store
            .lease(StreamId::new("a"))
            .expect("resuming an existing stream needs no new capacity");
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_reader_wakes_on_the_next_part() {
        let store = store();
        let lease = store.lease(stream()).expect("the store has room");
        let live = Arc::clone(lease.live());
        let start = live.revision();

        let reader = tokio::spawn(async move { live.wait_after(start).await });
        tokio::task::yield_now().await;
        lease.write(part(0, true));

        let revision = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("the reader wakes")
            .expect("the reader task succeeds");
        assert!(revision > start);
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_reader_wakes_when_the_stream_ends() {
        let store = store();
        let lease = store.lease(stream()).expect("the store has room");
        let live = Arc::clone(lease.live());
        let start = live.revision();

        let reader = tokio::spawn(async move { live.wait_after(start).await });
        tokio::task::yield_now().await;
        lease.end();

        tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("the reader wakes")
            .expect("the reader task succeeds");
        assert!(lease.live().is_ended());
    }
}
