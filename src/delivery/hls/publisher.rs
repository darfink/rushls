use std::{collections::HashSet, sync::Arc};

use thiserror::Error;

use crate::{
    domain::{Payload, StreamId},
    mux::{FinishReason, PackagedMedia, PackagedPresentation, PackagingRenditionId},
};

use super::{StoreFull, StoreWriteError, StreamLease, StreamStore, gzip::gzip, uri};

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
    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError>;

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
}

impl StorePublisherFactory {
    pub fn new(store: StreamStore) -> Self {
        Self { store }
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
        // Which renditions carry text is fixed for the publication, so the
        // question is answered once here rather than per published object.
        let text = presentation
            .renditions
            .iter()
            .filter(|rendition| uri::is_text(rendition.config.segment_format))
            .map(|rendition| rendition.packaging_rendition_id)
            .collect();
        Ok(Box::new(StorePublisher {
            lease: self.store.lease(stream.clone(), presentation)?,
            text,
        }))
    }
}

struct StorePublisher {
    lease: StreamLease,
    /// The renditions whose media is text, and so is worth compressing.
    text: HashSet<PackagingRenditionId>,
}

impl StorePublisher {
    /// The encoding delivery will serve this media under, if any.
    ///
    /// Computed on the way in, where the bytes are already in hand and the work
    /// happens once per object. A completed segment carries no payload of its
    /// own — it is served from the parts already published — so there is
    /// nothing here to encode.
    fn encoding(&self, media: &PackagedMedia) -> Option<Payload> {
        if !self.text.contains(&media.rendition_id()) {
            return None;
        }
        let payload = match media {
            PackagedMedia::Initialization(segment) => &segment.payload,
            PackagedMedia::Chunk(chunk) => &chunk.payload,
            PackagedMedia::Segment(segment) => &segment.payload,
            PackagedMedia::SegmentCompleted(_) => return None,
        };
        Some(Payload::from_bytes(gzip(payload.bytes())))
    }
}

impl HlsPublisher for StorePublisher {
    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
        let gzip = self.encoding(&media);
        match self.lease.write_encoded(media, gzip)? {
            true => Ok(PublishOutcome::Published),
            false => Ok(PublishOutcome::Superseded),
        }
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
    use crate::{
        domain::Payload,
        media::fixtures::video_presentation,
        mux::{
            InitializationSegment, PackagedChunk, PackagedMedia, PackagedPresentation,
            PackagingRenditionId, PackagingSegmentId,
            fixtures::{RenditionBuilder, presentation},
        },
    };

    use super::{super::StoreLimits, *};

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
            Err(HlsError::Capacity(StoreFull { maximum: 1 }))
        ));
    }
}
