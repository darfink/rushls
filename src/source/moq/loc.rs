//! LOC and legacy Hang frames from one moq-lite track.
//!
//! A media track contains groups of frames. The first frame in each group is
//! a random access point. The reader consumes whole groups, requests ordered
//! delivery, buffers reordered groups, and fails on a persistent sequence gap
//! instead of silently omitting media.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use bytes::Bytes;
use moq_net::{Timescale, Timestamp};

use super::ReadError;

/// LOC's catalog convention: microseconds unless a frame says otherwise.
const DEFAULT_TIMESCALE: Timescale = Timescale::MICRO;

/// One decoded LOC frame.
pub struct Frame {
    pub timestamp: Timestamp,
    pub payload: Bytes,
    /// Whether a decoder may start here.
    ///
    /// LOC carries no such flag. A moq-lite group begins at a decodable point
    /// by protocol invariant, so the first frame of each group is the random
    /// access point and the rest are not.
    pub keyframe: bool,
}

/// Reads one media track without abandoning its current group.
pub struct Reader {
    subscriber: moq_net::track::Subscriber,
    group: Option<moq_net::group::Consumer>,
    /// Position within the open group, which is what names the keyframe.
    index: usize,
    legacy: bool,
    /// Whether an empty payload is media rather than a marker.
    cues: bool,
    next_sequence: Option<u64>,
    pending: BTreeMap<u64, moq_net::group::Consumer>,
    gap_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl Reader {
    pub fn new(subscriber: moq_net::track::Subscriber, legacy: bool) -> Self {
        Self {
            subscriber,
            group: None,
            index: 0,
            legacy,
            cues: false,
            next_sequence: None,
            pending: BTreeMap::new(),
            gap_deadline: None,
        }
    }

    /// What to ask for when subscribing to a media track.
    ///
    /// Arrival order with a 30 s age budget. The dependency abandons a group
    /// once a newer one is older than the budget allows, and its default budget
    /// is zero; this reader reorders groups itself and fails on a real gap, so
    /// the dependency must not give up on a late group first.
    pub fn subscription() -> moq_net::track::Subscription {
        moq_net::track::Subscription::default().with_max_age(Duration::from_secs(30))
    }

    /// Reads a text track, where an empty cue is how a publisher clears the
    /// display. Legacy's end marker only exists on audio and video, so here the
    /// same bytes are a cue and must reach the WebVTT writer.
    pub fn cues(mut self) -> Self {
        self.cues = true;
        self
    }

    /// The next frame, `None` once the publisher finishes the track.
    pub fn poll_read(&mut self, waiter: &kio::Waiter) -> Poll<Result<Option<Frame>, ReadError>> {
        loop {
            if self.group.is_none() {
                match ready!(self.poll_group(waiter))? {
                    Some(group) => {
                        self.next_sequence = group.sequence.checked_add(1);
                        self.group = Some(group);
                        self.index = 0;
                    }
                    None => return Poll::Ready(Ok(None)),
                }
            }
            let group = self.group.as_mut().expect("a group was just opened");
            let Some(frame) = ready!(group.poll_read_frame(waiter))? else {
                // The group ended; the next one starts at a keyframe again.
                self.group = None;
                continue;
            };
            let keyframe = self.index == 0;
            self.index = self.index.saturating_add(1);
            // Legacy uses an empty payload as an end marker, not a sample.
            let decoded = if self.legacy {
                decode_legacy(frame.payload, keyframe)?
            } else {
                decode(&frame.payload, keyframe)?
            };
            if decoded.payload.is_empty() && !self.cues {
                continue;
            }
            return Poll::Ready(Ok(Some(decoded)));
        }
    }
    /// QUIC streams arrive independently. The dependency's `next_group` skips
    /// late sequences, so receive in arrival order and wait for a missing group.
    fn poll_group(
        &mut self,
        waiter: &kio::Waiter,
    ) -> Poll<Result<Option<moq_net::group::Consumer>, ReadError>> {
        if let Some(next) = self.next_sequence
            && let Some(group) = self.pending.remove(&next)
        {
            self.gap_deadline = None;
            return Poll::Ready(Ok(Some(group)));
        }
        loop {
            if !self.pending.is_empty() && self.gap_deadline.is_none() {
                self.gap_deadline = Some(Box::pin(tokio::time::sleep(Duration::from_secs(1))));
            }
            if let Some(deadline) = &mut self.gap_deadline
                && deadline
                    .as_mut()
                    .poll(&mut Context::from_waker(waiter.waker()))
                    .is_ready()
            {
                return Poll::Ready(Err(ReadError::Malformed(
                    "MOQ media group missing after reorder deadline".into(),
                )));
            }
            match self.subscriber.poll_recv_group(waiter) {
                Poll::Ready(Ok(Some(group))) => {
                    let next = *self.next_sequence.get_or_insert(group.sequence);
                    if group.sequence < next {
                        continue;
                    }
                    if group.sequence == next {
                        self.gap_deadline = None;
                        return Poll::Ready(Ok(Some(group)));
                    }
                    // Bound both retained group handles and time spent waiting.
                    // Payload storage remains subject to the MOQ cache budget.
                    if self.pending.len() >= 256 {
                        return Poll::Ready(Err(ReadError::Malformed(
                            "MOQ group reorder buffer exceeded 256 groups".into(),
                        )));
                    }
                    self.pending.insert(group.sequence, group);
                    self.gap_deadline.get_or_insert_with(|| {
                        Box::pin(tokio::time::sleep(Duration::from_secs(1)))
                    });
                }
                Poll::Ready(Ok(None)) if !self.pending.is_empty() => {
                    return Poll::Ready(Err(ReadError::Malformed(
                        "MOQ track ended with a missing media group".into(),
                    )));
                }
                other => return other.map(|result| result.map_err(ReadError::from)),
            }
        }
    }
}

fn decode(payload: &Bytes, keyframe: bool) -> Result<Frame, ReadError> {
    let frame = moq_loc::decode(payload.clone())
        .map_err(|error| ReadError::Malformed(format!("malformed LOC frame: {error}").into()))?;
    // `decode` refuses a zero timescale, so any override here is usable.
    let timescale = frame
        .timescale
        .and_then(|value| Timescale::new(value).ok())
        .unwrap_or(DEFAULT_TIMESCALE);
    let timestamp = Timestamp::new(frame.timestamp, timescale)
        .map_err(|_| ReadError::Malformed("a LOC timestamp is out of range".into()))?;
    Ok(Frame {
        timestamp,
        payload: frame.payload,
        keyframe,
    })
}

fn decode_legacy(mut payload: Bytes, keyframe: bool) -> Result<Frame, ReadError> {
    let timestamp: u64 = moq_net::VarInt::decode_quic(&mut payload)
        .map_err(|error| {
            ReadError::Malformed(format!("malformed legacy timestamp: {error}").into())
        })?
        .into();
    Ok(Frame {
        timestamp: Timestamp::from_micros(timestamp)
            .map_err(|_| ReadError::Malformed("legacy timestamp out of range".into()))?,
        payload,
        keyframe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_timestamp_is_stripped_without_copying_the_bitstream()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut bytes = bytes::BytesMut::new();
        moq_net::VarInt::from_u64(123_456)
            .expect("fits")
            .encode_quic(&mut bytes)?;
        bytes.extend_from_slice(&[0x65, 0x88]);
        let frame = decode_legacy(bytes.freeze(), true)?;
        assert_eq!(frame.timestamp, Timestamp::from_micros(123_456)?);
        assert_eq!(frame.payload.as_ref(), &[0x65, 0x88]);
        assert!(frame.keyframe);
        assert!(decode_legacy(Bytes::from_static(&[0xff]), false).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn missing_groups_are_not_silently_skipped() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fixture, _source) = super::super::fixtures::Fixture::new();
        let consumer = fixture.track("video").consume();
        let subscriber = consumer.subscribe(Reader::subscription()).await?;
        let mut reader = Reader::new(subscriber, false);
        fixture.publish_frame("video", 0, &[1]);
        assert!(
            kio::wait(|waiter| reader.poll_read(waiter))
                .await?
                .is_some()
        );
        let mut group = fixture
            .track("video")
            .create_group(moq_net::group::Info { sequence: 2 })?;
        group.write_frame(Timestamp::ZERO, moq_loc::encode(1, &[2])?)?;
        group.finish()?;
        fixture.finish_media();
        assert!(kio::wait(|waiter| reader.poll_read(waiter)).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn late_groups_are_delivered_in_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fixture, _) = super::super::fixtures::Fixture::new();
        let subscriber = fixture
            .track("video")
            .consume()
            .subscribe(Reader::subscription())
            .await?;
        let mut reader = Reader::new(subscriber, false);
        fixture.publish_frame("video", 0, &[1]);
        kio::wait(|waiter| reader.poll_read(waiter)).await?;
        let mut later = fixture
            .track("video")
            .create_group(moq_net::group::Info { sequence: 2 })?;
        later.write_frame(Timestamp::ZERO, moq_loc::encode(2, &[3])?)?;
        later.finish()?;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                kio::wait(|waiter| reader.poll_read(waiter))
            )
            .await
            .is_err()
        );
        let mut missing = fixture
            .track("video")
            .create_group(moq_net::group::Info { sequence: 1 })?;
        missing.write_frame(Timestamp::ZERO, moq_loc::encode(1, &[2])?)?;
        missing.finish()?;
        for value in [2, 3] {
            let frame = kio::wait(|waiter| reader.poll_read(waiter))
                .await?
                .expect("ordered frame");
            assert_eq!(frame.payload.as_ref(), &[value]);
            assert!(frame.keyframe);
        }
        Ok(())
    }
    #[tokio::test]
    async fn a_second_gap_gets_its_own_deadline() -> Result<(), Box<dyn std::error::Error>> {
        let (mut fixture, _) = super::super::fixtures::Fixture::new();
        let subscriber = fixture
            .track("video")
            .consume()
            .subscribe(Reader::subscription())
            .await?;
        let mut reader = Reader::new(subscriber, false);
        fixture.publish_frame("video", 0, &[1]);
        kio::wait(|waiter| reader.poll_read(waiter)).await?;
        for sequence in [2, 4] {
            let mut group = fixture
                .track("video")
                .create_group(moq_net::group::Info { sequence })?;
            group.write_frame(Timestamp::ZERO, moq_loc::encode(sequence, &[1])?)?;
            group.finish()?;
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                kio::wait(|waiter| reader.poll_read(waiter))
            )
            .await
            .is_err()
        );
        let mut group = fixture
            .track("video")
            .create_group(moq_net::group::Info { sequence: 1 })?;
        group.write_frame(Timestamp::ZERO, moq_loc::encode(1, &[1])?)?;
        group.finish()?;
        for _ in 0..2 {
            kio::wait(|waiter| reader.poll_read(waiter)).await?;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            kio::wait(|waiter| reader.poll_read(waiter)),
        )
        .await?;
        assert!(matches!(result, Err(ReadError::Malformed(_))));
        Ok(())
    }
}
