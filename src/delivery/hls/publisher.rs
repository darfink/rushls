use std::{collections::HashSet, sync::Arc};

use thiserror::Error;

use crate::{
    domain::{BoxFuture, Payload, StreamId},
    mux::{
        ClosedCaptionService, FinishReason, PackagedMedia, PackagedPresentation,
        PackagingRenditionId,
    },
    observe::{Events, StreamEvent},
};

use super::{StoreFull, StoreWriteError, StreamLease, StreamStore, gzip::gzip};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HlsError {
    #[error("failed to initialize HLS publication: {0}")]
    Initialization(Box<str>),
    #[error(transparent)]
    Capacity(#[from] StoreFull),
    #[error(transparent)]
    Store(#[from] StoreWriteError),
    #[error("HLS publication failed: {0}")]
    Publication(Box<str>),
}

/// What became of one published object.
///
/// Distinguished from an error because a discarded write is not a fault — the
/// session that produced it is being handed over and is about to be told so.
/// But it is not a delivery either, and reporting it as one would credit a
/// displaced publisher with media no viewer can fetch, both in the part
/// counters and in the liveness signal those counters feed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published,
    /// Dropped: a successor holds this stream now.
    Superseded,
}

/// Accepts packaged media on behalf of one stream and makes it fetchable.
pub trait HlsPublisher: Send {
    /// Disarms cadence deadlines before the session flushes its accepted tail.
    fn publisher_disconnected(&mut self) {}

    /// Fast path avoids allocating a wait future when the publisher is ready.
    fn is_backpressured(&self) -> bool {
        false
    }

    /// Waits for retention I/O before accepting another bounded mux batch.
    /// Cancellation leaves all accepted media in the store.
    fn ready(&mut self) -> BoxFuture<'_, Result<(), HlsError>> {
        Box::pin(async { Ok(()) })
    }

    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError>;

    /// Advertises the in-band caption services this publication carries.
    ///
    /// Separate from the presentation handed to
    /// [`HlsPublisherFactory::publish`] because in-band captions cannot be
    /// described before media flows: nothing in a track's declared parameters
    /// says whether its access units carry caption SEI, so the answer only
    /// exists once packets have been inspected.
    ///
    /// Reports whether this changed what viewers are told, so a caller driving
    /// it from a per-packet observation can log a transition without tracking
    /// what it last declared. The default does nothing, which keeps publishers
    /// that do not model a topology from having to care.
    fn declare_closed_captions(&mut self, services: Arc<[ClosedCaptionService]>) -> bool {
        let _ = services;
        false
    }

    /// Closes out the publication.
    ///
    /// Only [`FinishReason::Final`] marks the stream complete and stops readers
    /// blocking for new media. The other two relinquish the publication and
    /// leave the stream running — a successor is already publishing, or the
    /// publisher may yet come back — and telling viewers the stream ended would
    /// be a lie that costs them a reconnect.
    ///
    /// Synchronous, and must stay prompt — it runs on the drain path of a
    /// session that may already have been cancelled.
    fn finish(&mut self, reason: FinishReason) -> Result<(), HlsError>;
}

/// Opens a publication for one muxer-described presentation.
///
/// Takes the stream identity because publishing is what makes a session
/// reachable by viewers; a publisher that could not name its stream would leave
/// the delivery side with no way to find it. The complete descriptor arrives
/// here rather than piecemeal as media events, so topology replacement and
/// durable rendition mapping are committed before an initialization is visible.
pub trait PublisherFactory: Send + Sync {
    fn start(
        &self,
        stream: &StreamId,
        presentation: Arc<PackagedPresentation>,
    ) -> Result<Box<dyn HlsPublisher>, HlsError>;
}

/// Publishes into a shared [`StreamStore`].
#[derive(Clone, Debug)]
pub struct StorePublisherFactory {
    store: StreamStore,
    /// Present in a running node. Kept optional so this publisher remains
    /// usable as a standalone delivery component with no observability sink.
    events: Option<Events>,
    timing: Option<(super::DurationRule, super::DurationRule)>,
}

impl StorePublisherFactory {
    pub fn new(store: StreamStore) -> Self {
        Self {
            store,
            events: None,
            timing: None,
        }
    }

    /// Announces availability on the media-writing task immediately after the
    /// first playable commit, before that same task can end its session.
    #[must_use]
    pub fn with_events(mut self, events: Events) -> Self {
        self.events = Some(events);
        self
    }

    /// Validate fixed operator budgets against the actual admitted contract.
    #[must_use]
    pub fn with_timing(
        mut self,
        hold_back: super::DurationRule,
        retain: super::DurationRule,
    ) -> Self {
        self.timing = Some((hold_back, retain));
        self
    }

    pub fn store(&self) -> &StreamStore {
        &self.store
    }
}

impl PublisherFactory for StorePublisherFactory {
    fn start(
        &self,
        stream: &StreamId,
        presentation: Arc<PackagedPresentation>,
    ) -> Result<Box<dyn HlsPublisher>, HlsError> {
        if let Some((hold_back, retain)) = self.timing {
            // The same presentation-wide value every media playlist will
            // advertise, so a fixed budget is judged against what viewers see.
            let target_duration =
                crate::delivery::store::PlaylistContract::presentation_target_duration(
                    presentation
                        .renditions
                        .iter()
                        .map(|rendition| &rendition.config),
                );
            for rendition in presentation.renditions.iter() {
                let Some(target_duration) = target_duration else {
                    break;
                };
                let contract = crate::delivery::store::PlaylistContract::derive(
                    &rendition.config,
                    target_duration,
                )
                .ok_or_else(|| HlsError::Publication("inconsistent presentation target".into()))?;
                if contract
                    .part_target
                    .is_some_and(|part| hold_back.resolve(part) < part.saturating_mul(2))
                {
                    return Err(HlsError::Initialization(
                        "hold-back is below twice the selected part target".into(),
                    ));
                }
                let segment = contract.target_duration();
                if retain.resolve(segment) < segment.saturating_mul(3) {
                    return Err(HlsError::Initialization(
                        "retention is below three selected segment targets".into(),
                    ));
                }
            }
        }
        // Which renditions carry text is fixed for the publication, so the
        // question is answered once here rather than per published object.
        let text = presentation
            .renditions
            .iter()
            .filter(|rendition| crate::delivery::uri::is_text(rendition.config.segment_format))
            .map(|rendition| rendition.packaging_rendition_id)
            .collect();
        Ok(Box::new(StorePublisher {
            lease: self.store.lease(stream.clone(), &presentation)?,
            text,
            events: self.events.clone(),
        }))
    }
}

struct StorePublisher {
    lease: StreamLease,
    /// The renditions whose media is text, and so is worth compressing.
    text: HashSet<PackagingRenditionId>,
    events: Option<Events>,
}

impl StorePublisher {
    /// The encoding delivery will serve this media under, if any.
    ///
    /// Computed on the way in, where the bytes are already in hand and the work
    /// happens once per object. A completed segment carries no payload of its
    /// own because delivery serves it from the parts already published.
    fn encoding(&self, media: &PackagedMedia) -> Option<Payload> {
        if !self.text.contains(&media.rendition_id()) {
            return None;
        }
        let payload = match media {
            PackagedMedia::Initialization(segment) => &segment.payload,
            PackagedMedia::Chunk(chunk) => &chunk.payload,
            PackagedMedia::Segment(segment) => &segment.payload,
            PackagedMedia::SegmentCompleted(_) | PackagedMedia::Gap(_) => return None,
        };
        Some(Payload::from_bytes(gzip(&payload.bytes())))
    }
}

impl HlsPublisher for StorePublisher {
    fn publisher_disconnected(&mut self) {
        self.lease.publisher_disconnected();
    }

    fn is_backpressured(&self) -> bool {
        self.lease.live().is_backpressured()
    }

    fn ready(&mut self) -> BoxFuture<'_, Result<(), HlsError>> {
        Box::pin(async { self.lease.ready().await.map_err(Into::into) })
    }

    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
        let gzip = self.encoding(&media);
        if self.lease.write_encoded(media.into_retained(), gzip)? {
            // The store commit has released its mutation lock by here.
            // Emitting inline gives this transition causal order with the
            // later `session.ended` emitted by the same session task.
            if let Some(events) = &self.events
                && self.lease.live().claim_availability()
            {
                events.stream(self.lease.stream().clone(), StreamEvent::Available);
            }
            Ok(PublishOutcome::Published)
        } else {
            Ok(PublishOutcome::Superseded)
        }
    }

    fn declare_closed_captions(&mut self, services: Arc<[ClosedCaptionService]>) -> bool {
        self.lease.declare_closed_captions(services)
    }

    fn finish(&mut self, reason: FinishReason) -> Result<(), HlsError> {
        if matches!(reason, FinishReason::Final) {
            self.lease.end();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use parking_lot::Mutex;

    use crate::{
        domain::{Payload, SessionId},
        media::fixtures::video_presentation,
        mux::{
            InitializationSegment, PackagedChunk, PackagedMedia, PackagedPresentation,
            PackagingRenditionId, PackagingSegmentId,
            fixtures::{RenditionBuilder, presentation},
        },
        observe::{EventObserver, SessionEvent},
    };

    use super::{super::StoreLimits, *};

    #[derive(Clone, Default)]
    struct StreamLog(Arc<Mutex<Vec<(StreamId, StreamEvent)>>>);

    impl EventObserver for StreamLog {
        fn observe(&self, _session: SessionId, _event: SessionEvent) {}

        fn observe_stream(&self, stream: StreamId, event: StreamEvent) {
            self.0.lock().push((stream, event));
        }
    }

    fn packaged_presentation() -> Arc<PackagedPresentation> {
        Arc::new(presentation(
            &video_presentation(),
            vec![
                RenditionBuilder::new(0, crate::domain::MediaKind::Video)
                    .key("video/main")
                    .build(),
            ],
        ))
    }

    fn initialization() -> PackagedMedia {
        PackagedMedia::Initialization(InitializationSegment {
            rendition_id: PackagingRenditionId(0),
            version: 1,
            payload: Payload::from(vec![0]),
        })
    }

    fn chunk() -> PackagedMedia {
        PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(0),
            packaging_segment_id: PackagingSegmentId(0),
            chunk_index: 0,
            media_start: 0,
            duration: 90_000,
            independent: true,
            payload: Payload::from(vec![7]),
        })
    }

    fn start_media(publisher: &mut dyn HlsPublisher) {
        publisher
            .write(initialization())
            .expect("initialization is published");
        publisher.write(chunk()).expect("chunk is published");
    }

    #[test]
    fn first_playable_commit_announces_availability_inline() -> Result<(), Box<dyn Error>> {
        let store = StreamStore::default();
        let log = StreamLog::default();
        let factory = StorePublisherFactory::new(store.clone())
            .with_events(Events::new(Arc::new(log.clone())));
        let stream = StreamId::new("live/camera");
        let mut publisher = factory.start(&stream, packaged_presentation())?;

        publisher.write(initialization())?;
        assert!(log.0.lock().is_empty(), "a header alone is not playable");
        publisher.write(chunk())?;

        assert_eq!(
            *log.0.lock(),
            vec![(stream, StreamEvent::Available)],
            "availability is emitted before the writing task can end its session"
        );
        assert!(
            !store
                .get(&StreamId::new("live/camera"))
                .ok_or("stream is retained")?
                .claim_availability(),
            "the inline announcement owns the latch permanently"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn publishing_makes_a_stream_readable_and_finishing_closes_it() {
        let store = StreamStore::default();
        let factory = StorePublisherFactory::new(store.clone());
        let stream = StreamId::new("live/camera");

        let mut publisher = factory
            .start(&stream, packaged_presentation())
            .expect("publication starts");
        start_media(&mut *publisher);

        let live = store.get(&stream).expect("stream is readable");
        assert_eq!(
            live.snapshot().renditions[0]
                .snapshot()
                .open_segment
                .as_ref()
                .map(|segment| segment.parts.len()),
            Some(1)
        );
        assert!(!live.is_ended());

        publisher
            .finish(FinishReason::Final)
            .expect("publication finishes");
        assert!(live.is_ended());
    }

    #[tokio::test(start_paused = true)]
    async fn a_superseded_publisher_leaves_the_stream_running_for_its_successor() {
        let store = StreamStore::default();
        let factory = StorePublisherFactory::new(store.clone());
        let stream = StreamId::new("live/camera");

        let mut incumbent = factory
            .start(&stream, packaged_presentation())
            .expect("publication starts");
        start_media(&mut *incumbent);
        let mut successor = factory
            .start(&stream, packaged_presentation())
            .expect("takeover starts");

        assert_eq!(
            incumbent.write(chunk()),
            Ok(PublishOutcome::Superseded),
            "a drained tail is discarded, and the caller is told so rather than \
             counting it as delivered"
        );
        incumbent
            .finish(FinishReason::Superseded)
            .expect("the incumbent relinquishes");
        drop(incumbent);

        let live = store.get(&stream).expect("stream is still readable");
        assert!(!live.is_ended());
        assert!(!live.is_idle());

        successor
            .write(initialization())
            .expect("successor header is published");
        successor
            .write(chunk())
            .expect("successor chunk is published");
        assert_eq!(
            live.snapshot().renditions[0]
                .snapshot()
                .open_segment
                .as_ref()
                .map(|segment| segment.parts.len()),
            Some(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupted_publisher_leaves_the_stream_open_for_its_return() {
        let store = StreamStore::default();
        let factory = StorePublisherFactory::new(store.clone());
        let stream = StreamId::new("live/camera");

        let mut publisher = factory
            .start(&stream, packaged_presentation())
            .expect("publication starts");
        start_media(&mut *publisher);
        publisher
            .finish(FinishReason::Interrupted)
            .expect("the publication is relinquished");

        let live = store.get(&stream).expect("stream is retained");
        assert!(
            !live.is_ended(),
            "a dropped connection is not an end of stream; viewers keep waiting \
             through the reconnect budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_store_rejects_the_publication_rather_than_evicting_a_stream() {
        let store = StreamStore::new(StoreLimits {
            maximum_streams: 1,
            ..StoreLimits::default()
        });
        let factory = StorePublisherFactory::new(store);

        let _held = factory
            .start(&StreamId::new("a"), packaged_presentation())
            .expect("the first publication fits");

        assert!(matches!(
            factory.start(&StreamId::new("b"), packaged_presentation()),
            Err(HlsError::Capacity(StoreFull::Streams { maximum: 1 }))
        ));
    }
}
