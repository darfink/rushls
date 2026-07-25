use thiserror::Error;

use crate::{
    domain::StreamId,
    media::PresentationPlan,
    mux::{FinishReason, MuxedMedia},
};

use super::{StoreFull, StreamLease, StreamStore};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HlsError {
    #[error("failed to initialize HLS publication: {0}")]
    Initialization(String),
    #[error(transparent)]
    Capacity(#[from] StoreFull),
    #[error("HLS publication failed: {0}")]
    Publication(String),
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

/// Accepts muxed media on behalf of one stream and makes it fetchable.
pub trait HlsPublisher: Send {
    fn write(&mut self, media: MuxedMedia) -> Result<PublishOutcome, HlsError>;

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

/// Opens a publication for one validated presentation.
///
/// Takes the stream identity because publishing is what makes a session
/// reachable by viewers; a publisher that could not name its stream would leave
/// the delivery side with no way to find it.
pub trait PublisherFactory: Send + Sync {
    fn start(
        &self,
        stream: &StreamId,
        presentation: &PresentationPlan,
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
        _presentation: &PresentationPlan,
    ) -> Result<Box<dyn HlsPublisher>, HlsError> {
        Ok(Box::new(StorePublisher {
            lease: self.store.lease(stream.clone())?,
        }))
    }
}

struct StorePublisher {
    lease: StreamLease,
}

impl HlsPublisher for StorePublisher {
    fn write(&mut self, media: MuxedMedia) -> Result<PublishOutcome, HlsError> {
        match self.lease.write(media) {
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
        admission::StreamPolicy,
        domain::{
            Codec, DiscoveredTrack, FrameRate, MediaParameters, Payload, RenditionId, Timebase,
            TrackCatalog, TrackId,
        },
        media::validate,
        mux::MuxedPart,
    };

    use super::{super::StoreLimits, *};

    fn presentation() -> PresentationPlan {
        let catalog = TrackCatalog::new(vec![DiscoveredTrack {
            id: TrackId(0),
            codec: Codec::H264,
            parameters: MediaParameters::Video {
                width: nz::u32!(1920),
                height: nz::u32!(1080),
                frame_rate: Some(FrameRate::new(nz::u32!(30), nz::u32!(1))),
                video_delay: 0,
            },
            timebase: Timebase::hz90k(),
            first_pts: Some(0),
            title: None,
            language: None,
            codec_extradata: Payload::default(),
        }])
        .expect("test catalog is valid");
        validate(&catalog, &StreamPolicy::permissive()).expect("test presentation is valid")
    }

    fn part() -> MuxedMedia {
        MuxedMedia::Part(MuxedPart {
            rendition_id: RenditionId(0),
            media_start: 0,
            duration: 18_000,
            independent: true,
            payload: Payload::from(vec![7]),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn publishing_makes_a_stream_readable_and_finishing_closes_it() {
        let store = StreamStore::default();
        let factory = StorePublisherFactory::new(store.clone());
        let stream = StreamId::new("live/camera");

        let mut publisher = factory
            .start(&stream, &presentation())
            .expect("publication starts");
        publisher.write(part()).expect("part is published");

        let live = store.get(&stream).expect("stream is readable");
        assert_eq!(live.snapshot().renditions[0].parts.len(), 1);
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
            .start(&stream, &presentation())
            .expect("publication starts");
        incumbent.write(part()).expect("part is published");
        let mut successor = factory
            .start(&stream, &presentation())
            .expect("takeover starts");

        assert_eq!(
            incumbent.write(part()),
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

        assert_eq!(
            successor.write(part()),
            Ok(PublishOutcome::Published),
            "the successor owns the stream from the instant it leased it"
        );
        assert_eq!(live.snapshot().renditions[0].parts.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupted_publisher_leaves_the_stream_open_for_its_return() {
        let store = StreamStore::default();
        let factory = StorePublisherFactory::new(store.clone());
        let stream = StreamId::new("live/camera");

        let mut publisher = factory
            .start(&stream, &presentation())
            .expect("publication starts");
        publisher.write(part()).expect("part is published");
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
            .start(&StreamId::new("a"), &presentation())
            .expect("the first publication fits");

        assert!(matches!(
            factory.start(&StreamId::new("b"), &presentation()),
            Err(HlsError::Capacity(StoreFull { maximum: 1 }))
        ));
    }
}
