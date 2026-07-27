use std::sync::Arc;

use thiserror::Error;

use crate::{
    domain::StreamId,
    mux::{FinishReason, PackagedMedia, PackagedPresentation},
};

use super::{StoreFull, StoreWriteError, StreamLease, StreamStore};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HlsError {
    #[error("failed to initialize HLS publication: {0}")]
    Initialization(String),
    #[error(transparent)]
    Capacity(#[from] StoreFull),
    #[error(transparent)]
    Store(#[from] StoreWriteError),
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
        Ok(Box::new(StorePublisher {
            lease: self.store.lease(stream.clone(), presentation)?,
        }))
    }
}

struct StorePublisher {
    lease: StreamLease,
}

impl HlsPublisher for StorePublisher {
    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
        match self.lease.write(media)? {
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
    use std::{num::NonZero, time::SystemTime};

    use crate::{
        admission::StreamPolicy,
        domain::{
            Codec, DiscoveredTrack, FrameRate, MediaKind, MediaParameters, Payload, Timebase,
            TrackCatalog, TrackId,
        },
        media::{PresentationPlan, validate},
        mux::{
            InitializationSegment, MediaSegmentFormat, PackagedChunk, PackagedMedia,
            PackagedPresentation, PackagedRendition, PackagingRenditionId, PackagingSegmentId,
            PlayableCombination, RenditionConfig, RenditionGroup, RenditionGroupKey, RenditionKey,
            RenditionMedia,
        },
    };

    use super::{super::StoreLimits, *};

    fn presentation() -> PresentationPlan {
        let catalog = TrackCatalog::new(vec![DiscoveredTrack {
            id: TrackId(0),
            source_key: None,
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

    fn packaged_presentation() -> Arc<PackagedPresentation> {
        let input = presentation();
        let rendition_id = PackagingRenditionId(0);
        Arc::new(
            PackagedPresentation::new(
                SystemTime::UNIX_EPOCH,
                &input,
                vec![PackagedRendition {
                    packaging_rendition_id: rendition_id,
                    key: RenditionKey::new("video/main"),
                    source_tracks: Arc::from([TrackId(0)]),
                    config: RenditionConfig {
                        timebase: Timebase::hz90k(),
                        segment_target: NonZero::new(540_000).unwrap(),
                        chunk_target: NonZero::new(90_000),
                        segment_format: MediaSegmentFormat::Cmaf,
                    },
                    media: RenditionMedia::Video {
                        width: nz::u32!(1920),
                        height: nz::u32!(1080),
                        frame_rate: Some(FrameRate::new(nz::u32!(30), nz::u32!(1))),
                        video_range: None,
                    },
                    codecs: Arc::from("avc1.640028"),
                    name: Arc::from("Main"),
                    language: None,
                    is_default: true,
                    declared_bandwidth: None,
                }],
                vec![RenditionGroup {
                    key: RenditionGroupKey::new("video"),
                    media_kind: MediaKind::Video,
                    renditions: Arc::from([rendition_id]),
                }],
                vec![PlayableCombination {
                    groups: Arc::from([RenditionGroupKey::new("video")]),
                }],
            )
            .expect("test packaged presentation is valid"),
        )
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
