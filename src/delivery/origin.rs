//! The protocol-neutral read side of retained delivery state.
//!
//! Protocol adapters decide what a request means and how long it may wait. The
//! origin owns the mechanics they share: resolving immutable media objects,
//! validating packaging claims, and waiting on one rendition without losing
//! the distinction between reaching a condition and the stream ending.

use std::{sync::Arc, time::Duration};

use tokio::time::timeout;

use crate::{
    delivery::{
        DeliveryError, MediaBody,
        store::{
            LiveStream, PartId, PlaylistContract, RenditionLiveEdge, RenditionSnapshot, StreamStore,
        },
        uri::MediaResource,
    },
    domain::{Payload, RenditionId, StreamId},
    observe::OriginMeters,
};

/// Media retained under one durable resource identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaObject {
    pub body: MediaBody,
    pub gzip: Option<bytes::Bytes>,
    pub contract: PlaylistContract,
}

/// A condition a protocol adapter can wait for on one rendition.
pub trait EdgeCondition {
    fn reached(&self, edge: &RenditionLiveEdge) -> bool;
}

/// Why a rendition wait completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Reached,
    Ended,
}

#[derive(Clone, Copy)]
struct PartPublished(PartId);

impl EdgeCondition for PartPublished {
    fn reached(&self, edge: &RenditionLiveEdge) -> bool {
        edge.last_part
            .is_some_and(|(_, published)| published >= self.0)
    }
}

/// The streams and retained media this node makes available to viewers.
#[derive(Clone, Debug)]
pub struct Origin {
    store: StreamStore,
    meters: OriginMeters,
}

impl Origin {
    pub fn new(store: StreamStore) -> Self {
        Self {
            store,
            meters: OriginMeters::default(),
        }
    }

    pub fn meters(&self) -> &OriginMeters {
        &self.meters
    }

    pub fn stream(&self, stream: &StreamId) -> Option<Arc<LiveStream>> {
        self.store.get(stream)
    }

    /// Resolves one media object, waiting for a hinted part when necessary.
    ///
    /// The caller supplies the deadline because deciding it is protocol
    /// policy. Initialization sections and completed media never wait.
    pub async fn media(
        &self,
        stream: &StreamId,
        resource: MediaResource,
        deadline: Duration,
    ) -> Result<MediaObject, DeliveryError> {
        let result = self.resolve_media(stream, resource, deadline).await;
        match &result {
            Ok(object) => self.meters.media_served(object.body.len()),
            Err(
                DeliveryError::UnknownStream
                | DeliveryError::UnknownRendition
                | DeliveryError::UnknownResource,
            ) => self.meters.request_not_found(),
            Err(DeliveryError::InvalidDirective(_)) => self.meters.request_rejected(),
            Err(DeliveryError::Unsatisfied | DeliveryError::Projection) => {}
        }
        result
    }

    async fn resolve_media(
        &self,
        stream: &StreamId,
        resource: MediaResource,
        deadline: Duration,
    ) -> Result<MediaObject, DeliveryError> {
        let live = self.stream(stream).ok_or(DeliveryError::UnknownStream)?;
        let rendition = resource.rendition();
        let snapshot = self.rendition_for(&live, resource)?;
        let contract = snapshot.contract;

        let object = match resource {
            MediaResource::Initialization(_, initialization, _) => {
                let held = snapshot
                    .initialization_for(initialization)
                    .ok_or(DeliveryError::UnknownResource)?;
                MediaObject {
                    body: MediaBody::single(held.payload.clone()),
                    gzip: held.gzip.as_ref().map(Payload::bytes).cloned(),
                    contract,
                }
            }
            MediaResource::Segment(_, segment, _) => {
                let stored = live
                    .segment(rendition, segment)
                    .ok_or(DeliveryError::UnknownResource)?;
                MediaObject {
                    body: MediaBody::from_segment(&stored),
                    gzip: stored.gzip.as_ref().map(Payload::bytes).cloned(),
                    contract,
                }
            }
            MediaResource::Part(_, part, _) => {
                if let Some(stored) = live.part(rendition, part) {
                    MediaObject {
                        body: MediaBody::single(stored.payload.clone()),
                        gzip: stored.gzip.as_ref().map(Payload::bytes).cloned(),
                        contract,
                    }
                } else {
                    // Only the preload-hinted frontier is expected to arrive.
                    // An older absent part expired and a far-future identity was
                    // never promised, so neither deserves a parked request.
                    let hinted = snapshot
                        .live_edge
                        .next_part_id
                        .is_some_and(|next| part >= next);
                    if !hinted {
                        return Err(DeliveryError::UnknownResource);
                    }
                    let _outcome = self
                        .wait_for(&live, rendition, deadline, PartPublished(part))
                        .await?;
                    let stored = live
                        .part(rendition, part)
                        .ok_or(DeliveryError::UnknownResource)?;
                    MediaObject {
                        body: MediaBody::single(stored.payload.clone()),
                        gzip: stored.gzip.as_ref().map(Payload::bytes).cloned(),
                        contract,
                    }
                }
            }
        };
        Ok(object)
    }

    /// Waits on one rendition, reporting termination separately from reaching
    /// the requested condition.
    pub async fn wait_for<C: EdgeCondition>(
        &self,
        live: &Arc<LiveStream>,
        rendition: RenditionId,
        deadline: Duration,
        condition: C,
    ) -> Result<WaitOutcome, DeliveryError> {
        let mut updates = live
            .subscribe_rendition(rendition)
            .ok_or(DeliveryError::UnknownRendition)?;
        let wait = async {
            loop {
                let edge = *updates.borrow_and_update();
                if condition.reached(&edge) {
                    return Ok(WaitOutcome::Reached);
                }
                if edge.ended {
                    return Ok(WaitOutcome::Ended);
                }
                if updates.changed().await.is_err() {
                    return Err(DeliveryError::UnknownResource);
                }
            }
        };
        timeout(deadline, wait)
            .await
            .unwrap_or(Err(DeliveryError::Unsatisfied))
    }

    pub fn rendition_for(
        &self,
        live: &Arc<LiveStream>,
        resource: MediaResource,
    ) -> Result<Arc<RenditionSnapshot>, DeliveryError> {
        let rendition = live
            .rendition(resource.rendition())
            .ok_or(DeliveryError::UnknownRendition)?;
        (rendition.contract.segment_format == resource.format())
            .then_some(rendition)
            .ok_or(DeliveryError::UnknownResource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        delivery::{
            hls::fixtures::{
                PART_BYTES, chunk, initialization, lease, stream_id, video, write, write_segment,
            },
            store::{Msn, SegmentId},
        },
        mux::MediaSegmentFormat,
    };

    const CMAF: MediaSegmentFormat = MediaSegmentFormat::Cmaf;

    #[tokio::test(start_paused = true)]
    async fn chunked_media_is_shared_by_refcount_and_validated_by_format()
    -> Result<(), DeliveryError> {
        let store = StreamStore::default();
        let origin = Origin::new(store.clone());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write_segment(&lease, 0, 0, 0);

        let object = origin
            .media(
                &stream_id(),
                MediaResource::Segment(RenditionId(0), SegmentId(1), CMAF),
                Duration::ZERO,
            )
            .await?;
        assert_eq!(object.body.clone().into_frames().count(), 6);
        assert_eq!(object.body.len(), 6 * PART_BYTES as u64);
        assert_eq!(
            origin
                .media(
                    &stream_id(),
                    MediaResource::Segment(
                        RenditionId(0),
                        SegmentId(1),
                        MediaSegmentFormat::WebVtt,
                    ),
                    Duration::ZERO,
                )
                .await,
            Err(DeliveryError::UnknownResource)
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_hinted_part_waits_while_an_older_absence_does_not() -> Result<(), DeliveryError> {
        let store = StreamStore::default();
        let origin = Origin::new(store.clone());
        let lease = lease(&store, vec![video(0)]);
        write(&lease, initialization(0, 1));
        write(&lease, chunk(0, 0, 0, 0));

        let held = tokio::spawn({
            let origin = origin.clone();
            async move {
                origin
                    .media(
                        &stream_id(),
                        MediaResource::Part(RenditionId(0), PartId(2), CMAF),
                        Duration::from_secs(18),
                    )
                    .await
            }
        });
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!held.is_finished());

        write(&lease, chunk(0, 0, 1, 1));
        let object = held.await.expect("the waiter task ran")?;
        assert_eq!(object.body.len(), PART_BYTES as u64);
        assert_eq!(
            origin
                .media(
                    &stream_id(),
                    MediaResource::Part(RenditionId(0), PartId(0), CMAF),
                    Duration::from_secs(18),
                )
                .await,
            Err(DeliveryError::UnknownResource)
        );
        Ok(())
    }

    #[derive(Clone, Copy)]
    struct BeyondEnd;

    impl EdgeCondition for BeyondEnd {
        fn reached(&self, edge: &RenditionLiveEdge) -> bool {
            edge.last_segment.is_some_and(|(msn, _)| msn > Msn(99))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ending_is_distinct_from_reaching_the_waited_position() -> Result<(), DeliveryError> {
        let store = StreamStore::default();
        let origin = Origin::new(store.clone());
        let lease = lease(&store, vec![video(0)]);
        let live = origin
            .stream(&stream_id())
            .expect("the publication installed its stream");
        lease.end();

        assert_eq!(
            origin
                .wait_for(&live, RenditionId(0), Duration::from_secs(18), BeyondEnd,)
                .await?,
            WaitOutcome::Ended
        );
        Ok(())
    }
}
