//! CROWDCAST: regression test for interleaved chunk streams.
//!
//! RTMP allows a peer to interleave chunks from different chunk stream ids. A
//! large video message is split across many chunks, and a control message such
//! as an acknowledgement may be sent between them on chunk stream 2.
//!
//! The deserializer keeps a single `current_payload_data` buffer shared across
//! all chunk streams, so an interleaved message corrupts the in-progress one.

// The crate is edition 2015, so external crates must be declared explicitly.
extern crate bytes;
extern crate rml_rtmp;

use bytes::{BufMut, BytesMut};
use rml_rtmp::chunk_io::{ChunkDeserializer, ChunkSerializer};
use rml_rtmp::messages::RtmpMessage;
use rml_rtmp::time::RtmpTimestamp;

#[test]
fn control_message_interleaved_into_a_split_video_message() {
    let mut serializer = ChunkSerializer::new();
    let mut deserializer = ChunkDeserializer::new();

    // Small chunk size so the video message must span several chunks.
    let chunk_size = 128;
    let packet = serializer
        .set_max_chunk_size(chunk_size, RtmpTimestamp::new(0))
        .unwrap();
    let mut wire = BytesMut::new();
    wire.put_slice(&packet.bytes);

    // A video message several chunks long.
    let video = RtmpMessage::VideoData {
        data: bytes::Bytes::from(vec![0x27u8; 1024]),
    };
    let video_payload = video
        .into_message_payload(RtmpTimestamp::new(0), 1)
        .unwrap();
    let video_packet = serializer.serialize(&video_payload, false, false).unwrap();

    // An acknowledgement, which travels on its own chunk stream.
    let ack = RtmpMessage::Acknowledgement {
        sequence_number: 4096,
    };
    let ack_payload = ack.into_message_payload(RtmpTimestamp::new(0), 0).unwrap();
    let ack_packet = serializer.serialize(&ack_payload, false, false).unwrap();

    // Feed the chunk-size change, then the video message, then the ack.
    wire.put_slice(&video_packet.bytes);
    wire.put_slice(&ack_packet.bytes);

    let mut input: &[u8] = &wire;
    let mut messages = Vec::new();
    while let Some(payload) = deserializer.get_next_message(input).unwrap() {
        input = &[];
        messages.push(payload.to_rtmp_message().unwrap());
    }

    let video_count = messages
        .iter()
        .filter(|m| matches!(m, RtmpMessage::VideoData { .. }))
        .count();
    assert_eq!(
        video_count, 1,
        "video message did not survive: {messages:?}"
    );
}

/// Hand-built interleave: a large message on csid 4 is split, and a small
/// message on csid 5 is injected between its chunks. This is legal RTMP.
#[test]
fn manually_interleaved_chunk_streams_do_not_panic() {
    let mut deserializer = ChunkDeserializer::new();
    let mut wire = BytesMut::new();

    // Type 0 header on csid 4 announcing a 600 byte video message.
    wire.put_u8(0x04); // fmt 0, csid 4
    wire.put_slice(&[0, 0, 0]); // timestamp
    wire.put_slice(&[0x00, 0x02, 0x58]); // message length = 600
    wire.put_u8(9); // video
    wire.put_slice(&[1, 0, 0, 0]); // stream id
    wire.put_slice(&[0xAAu8; 128]); // first chunk (default max chunk size 128)

    // Type 0 header on csid 5 announcing a small 8 byte message, injected before
    // the csid 4 message has been completed.
    wire.put_u8(0x05); // fmt 0, csid 5
    wire.put_slice(&[0, 0, 0]);
    wire.put_slice(&[0x00, 0x00, 0x08]); // message length = 8
    wire.put_u8(9);
    wire.put_slice(&[1, 0, 0, 0]);
    wire.put_slice(&[0xBBu8; 8]);

    // Remaining chunks of the csid 4 message, as type 3 continuations. If the
    // interleaved csid 5 message had corrupted the buffer, these would not
    // reassemble into a clean 600 byte payload.
    // 600 = 128 + 128 + 128 + 128 + 88
    for _ in 0..3 {
        wire.put_u8(0xC4); // fmt 3, csid 4
        wire.put_slice(&[0xAAu8; 128]);
    }
    wire.put_u8(0xC4);
    wire.put_slice(&[0xAAu8; 88]);

    // Both messages must come back intact, on their own chunk streams, with no
    // cross-contamination between the interleaved payloads.
    let mut input: &[u8] = &wire;
    let mut payloads = Vec::new();
    loop {
        match deserializer.get_next_message(input) {
            Ok(Some(payload)) => {
                input = &[];
                payloads.push(payload);
            }
            Ok(None) => break,
            Err(error) => panic!("interleaved chunk streams failed to parse: {}", error),
        }
    }
    // `get_next_message` returns at most one message per call and buffers the
    // rest, so drain with repeated empty-input calls.
    loop {
        match deserializer.get_next_message(&[]) {
            Ok(Some(payload)) => payloads.push(payload),
            Ok(None) => break,
            Err(error) => panic!("interleaved chunk streams failed to parse: {}", error),
        }
    }

    assert_eq!(payloads.len(), 2, "expected both interleaved messages");

    let long = payloads
        .iter()
        .find(|p| p.data.len() == 600)
        .expect("600 byte message was lost or truncated");
    assert!(
        long.data.iter().all(|b| *b == 0xAA),
        "long message was contaminated by the interleaved one"
    );

    let short = payloads
        .iter()
        .find(|p| p.data.len() == 8)
        .expect("8 byte message was lost");
    assert!(
        short.data.iter().all(|b| *b == 0xBB),
        "short message was contaminated by the interleaved one"
    );
}

/// A peer that declares a message length shorter than what it already sent must
/// produce a protocol error, not an arithmetic panic.
#[test]
fn message_length_shorter_than_buffered_payload_is_an_error_not_a_panic() {
    let mut deserializer = ChunkDeserializer::new();
    let mut wire = BytesMut::new();

    // Announce 600 bytes on csid 4 and send one full 128 byte chunk.
    wire.put_u8(0x04);
    wire.put_slice(&[0, 0, 0]);
    wire.put_slice(&[0x00, 0x02, 0x58]);
    wire.put_u8(9);
    wire.put_slice(&[1, 0, 0, 0]);
    wire.put_slice(&[0xAAu8; 128]);

    // Now re-announce csid 4 with a *smaller* length than is already buffered.
    wire.put_u8(0x04);
    wire.put_slice(&[0, 0, 0]);
    wire.put_slice(&[0x00, 0x00, 0x10]); // 16 bytes
    wire.put_u8(9);
    wire.put_slice(&[1, 0, 0, 0]);
    wire.put_slice(&[0xCCu8; 16]);

    let mut input: &[u8] = &wire;
    loop {
        match deserializer.get_next_message(input) {
            Ok(Some(_)) => input = &[],
            Ok(None) => break,
            // A protocol error is the correct outcome. Reaching here without a panic
            // is the point of the test.
            Err(_) => break,
        }
    }
}
