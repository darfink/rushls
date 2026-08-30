use cc_rtmp::{
    chunk_io::{ChunkDeserializer, ChunkDeserializerConfig, ChunkSerializer},
    sessions::{ClientSession, ClientSessionConfig, ServerSession, ServerSessionConfig},
    time::RtmpTimestamp,
};

fn full_chunk(csid: u8, message_length: usize, payload: &[u8]) -> Vec<u8> {
    assert!((2..64).contains(&csid));
    assert!(message_length <= 0x00ff_ffff);
    let mut bytes = vec![
        csid,
        0,
        0,
        0,
        ((message_length >> 16) & 0xff) as u8,
        ((message_length >> 8) & 0xff) as u8,
        (message_length & 0xff) as u8,
        9,
        1,
        0,
        0,
        0,
    ];
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn rejects_zero_chunk_sizes_and_acknowledgement_windows() {
    assert!(ChunkDeserializer::new().set_max_chunk_size(0).is_err());
    assert!(
        ChunkSerializer::new()
            .set_max_chunk_size(0, RtmpTimestamp::new(0))
            .is_err()
    );

    let mut server = ServerSessionConfig::new();
    server.window_ack_size = 0;
    assert!(ServerSession::new(server).is_err());
    let mut client = ClientSessionConfig::new();
    client.window_ack_size = 0;
    assert!(ClientSession::new(client).is_err());
}

#[test]
fn rejects_oversized_declared_messages_before_allocating_the_payload() {
    let limits = ChunkDeserializerConfig {
        maximum_message_size: 32,
        ..ChunkDeserializerConfig::default()
    };
    let mut parser = ChunkDeserializer::with_config(limits);
    assert!(parser.get_next_message(&full_chunk(3, 33, &[])).is_err());
}

#[test]
fn bounds_tracked_chunk_stream_ids() {
    let limits = ChunkDeserializerConfig {
        maximum_tracked_chunk_streams: 1,
        ..ChunkDeserializerConfig::default()
    };
    let mut parser = ChunkDeserializer::with_config(limits);
    assert!(
        parser
            .get_next_message(&full_chunk(3, 1, &[1]))
            .unwrap()
            .is_some()
    );
    assert!(parser.get_next_message(&full_chunk(4, 1, &[2])).is_err());
}

#[test]
fn bounds_concurrent_partial_messages() {
    let limits = ChunkDeserializerConfig {
        maximum_partial_messages: 1,
        ..ChunkDeserializerConfig::default()
    };
    let mut parser = ChunkDeserializer::with_config(limits);
    assert!(
        parser
            .get_next_message(&full_chunk(3, 129, &[1; 128]))
            .unwrap()
            .is_none()
    );
    assert!(
        parser
            .get_next_message(&full_chunk(4, 129, &[2; 128]))
            .is_err()
    );
}

#[test]
fn bounds_total_buffered_wire_and_payload_bytes() {
    let limits = ChunkDeserializerConfig {
        maximum_buffered_bytes: 10,
        ..ChunkDeserializerConfig::default()
    };
    let mut parser = ChunkDeserializer::with_config(limits);
    assert!(parser.get_next_message(&[0; 11]).is_err());
}
