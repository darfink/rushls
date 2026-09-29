//! PROXY protocol headers, read ahead of a TCP stream.
//!
//! A load balancer or TLS terminator in front of the RTMP listener opens its
//! own connection, so the socket's peer is the proxy rather than the
//! publisher. The proxy writes one header naming the original client before
//! any application byte, and the rest of the stream is untouched RTMP.
//!
//! Both versions are accepted because proxies default to different ones:
//! v1 is a text line (nginx `stream`), v2 is binary (AWS NLB, HAProxy
//! `send-proxy-v2`). Only the addresses are used; v2 TLVs are skipped.
//!
//! Enabling this makes the header mandatory. An optional header would let
//! any client that can reach the listener directly claim any address.
//!
//! Specification: <https://www.haproxy.org/download/2.9/doc/proxy-protocol.txt>

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt};

const V2_SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// Longest legal v1 line, `\r\n` included.
const V1_MAXIMUM: usize = 107;
/// Addresses plus the TLVs real proxies attach fit far inside this; the
/// header length field alone would allow a peer to demand 64 KiB of reading.
const V2_MAXIMUM_PAYLOAD: usize = 1024;

/// Reads one header and returns the client it names.
///
/// `peer` is returned when the header carries no address: a v2 `LOCAL`
/// command (proxy health checks) or an `UNKNOWN`/unspecified family. Reads
/// exactly the header's bytes, so the stream resumes at the first byte the
/// client sent. The caller bounds the time this may take.
pub async fn read_header<S: AsyncRead + Unpin>(
    io: &mut S,
    peer: SocketAddr,
) -> Result<SocketAddr, String> {
    // Twelve bytes are enough to tell the versions apart: the shortest v1
    // line, "PROXY UNKNOWN\r\n", is fifteen.
    let mut start = [0; 12];
    io.read_exact(&mut start)
        .await
        .map_err(|error| read_error(&error))?;
    if start == V2_SIGNATURE {
        read_v2(io, peer).await
    } else if start.starts_with(b"PROXY ") {
        read_v1(io, start, peer).await
    } else {
        Err("expected a PROXY protocol header; ingest.rtmp.proxy_protocol requires one".into())
    }
}

fn read_error(error: &std::io::Error) -> String {
    format!("could not read the PROXY protocol header: {error}")
}

async fn read_v1<S: AsyncRead + Unpin>(
    io: &mut S,
    start: [u8; 12],
    peer: SocketAddr,
) -> Result<SocketAddr, String> {
    let mut line = start.to_vec();
    // Byte at a time: reading ahead would consume RTMP handshake bytes that
    // belong to the session. The line is at most 107 bytes.
    while !line.ends_with(b"\r\n") {
        if line.len() == V1_MAXIMUM {
            return Err("PROXY protocol v1 line is longer than 107 bytes".into());
        }
        line.push(io.read_u8().await.map_err(|error| read_error(&error))?);
    }
    let line = std::str::from_utf8(&line[..line.len() - 2])
        .map_err(|_| "PROXY protocol v1 line is not ASCII".to_owned())?;
    parse_v1(line, peer)
}

fn parse_v1(line: &str, peer: SocketAddr) -> Result<SocketAddr, String> {
    let invalid = || format!("malformed PROXY protocol v1 line {line:?}");
    let fields: Vec<&str> = line.split(' ').collect();
    match fields.as_slice() {
        // The rest of an UNKNOWN line is undefined and must be ignored.
        ["PROXY", "UNKNOWN", ..] => Ok(peer),
        [
            "PROXY",
            family,
            source,
            _destination,
            port,
            _destination_port,
        ] => {
            let port: u16 = port.parse().map_err(|_| invalid())?;
            let address = match *family {
                "TCP4" => source.parse::<Ipv4Addr>().map(Into::into),
                "TCP6" => source.parse::<Ipv6Addr>().map(Into::into),
                _ => return Err(invalid()),
            }
            .map_err(|_| invalid())?;
            Ok(SocketAddr::new(address, port))
        }
        _ => Err(invalid()),
    }
}

async fn read_v2<S: AsyncRead + Unpin>(io: &mut S, peer: SocketAddr) -> Result<SocketAddr, String> {
    let mut fixed = [0; 4];
    io.read_exact(&mut fixed)
        .await
        .map_err(|error| read_error(&error))?;
    let [version_command, family, high, low] = fixed;
    let length = usize::from(u16::from_be_bytes([high, low]));
    if version_command >> 4 != 2 {
        return Err(format!(
            "unsupported PROXY protocol version {}",
            version_command >> 4
        ));
    }
    if length > V2_MAXIMUM_PAYLOAD {
        return Err(format!(
            "PROXY protocol v2 header of {length} bytes exceeds {V2_MAXIMUM_PAYLOAD}"
        ));
    }
    // Consumed in full even when unused, so the stream resumes after it.
    let mut payload = vec![0; length];
    io.read_exact(&mut payload)
        .await
        .map_err(|error| read_error(&error))?;
    parse_v2(version_command & 0x0f, family, &payload, peer)
}

fn parse_v2(
    command: u8,
    family: u8,
    payload: &[u8],
    peer: SocketAddr,
) -> Result<SocketAddr, String> {
    const LOCAL: u8 = 0x0;
    const PROXY: u8 = 0x1;
    const TCP_IPV4: u8 = 0x11;
    const TCP_IPV6: u8 = 0x21;
    const UNSPECIFIED: u8 = 0x00;
    const UNIX_STREAM: u8 = 0x31;

    let short = || {
        format!(
            "PROXY protocol v2 addresses are truncated ({} bytes)",
            payload.len()
        )
    };
    match (command, family) {
        // The proxy's own connection, typically a health check.
        (LOCAL, _) | (PROXY, UNSPECIFIED | UNIX_STREAM) => Ok(peer),
        (PROXY, TCP_IPV4) => {
            let bytes: [u8; 12] = payload
                .get(..12)
                .ok_or_else(short)?
                .try_into()
                .map_err(|_| short())?;
            let address = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            Ok(SocketAddr::new(
                address.into(),
                u16::from_be_bytes([bytes[8], bytes[9]]),
            ))
        }
        (PROXY, TCP_IPV6) => {
            let bytes: [u8; 36] = payload
                .get(..36)
                .ok_or_else(short)?
                .try_into()
                .map_err(|_| short())?;
            let mut address = [0; 16];
            address.copy_from_slice(&bytes[..16]);
            Ok(SocketAddr::new(
                Ipv6Addr::from(address).into(),
                u16::from_be_bytes([bytes[32], bytes[33]]),
            ))
        }
        (PROXY, family) => Err(format!("PROXY protocol v2 family {family:#04x} is not TCP")),
        (command, _) => Err(format!(
            "unsupported PROXY protocol v2 command {command:#x}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Result = std::result::Result<(), Box<dyn std::error::Error>>;

    fn peer() -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 2], 40000))
    }

    /// Parses `header`, then returns the address and what the stream still holds.
    async fn read(bytes: &[u8]) -> std::result::Result<(SocketAddr, Vec<u8>), String> {
        let mut io = bytes;
        let address = read_header(&mut io, peer()).await?;
        Ok((address, io.to_vec()))
    }

    fn v2(command: u8, family: u8, payload: &[u8]) -> Vec<u8> {
        let mut header = V2_SIGNATURE.to_vec();
        header.push(0x20 | command);
        header.push(family);
        header.extend_from_slice(
            &u16::try_from(payload.len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );
        header.extend_from_slice(payload);
        header
    }

    #[tokio::test]
    async fn a_v1_line_names_the_client_and_leaves_the_stream_intact() -> Result {
        let (address, rest) =
            read(b"PROXY TCP4 203.0.113.7 10.0.0.5 51234 1935\r\n\x03rtmp").await?;
        assert_eq!(address, "203.0.113.7:51234".parse()?);
        assert_eq!(rest, b"\x03rtmp");

        let (address, _) = read(b"PROXY TCP6 2001:db8::7 2001:db8::1 443 1935\r\n").await?;
        assert_eq!(address, "[2001:db8::7]:443".parse()?);
        Ok(())
    }

    #[tokio::test]
    async fn a_v1_unknown_line_keeps_the_socket_peer() -> Result {
        let (address, rest) = read(b"PROXY UNKNOWN ffff:f...f 1 2\r\nx").await?;
        assert_eq!(address, peer());
        assert_eq!(rest, b"x");
        Ok(())
    }

    #[tokio::test]
    async fn a_v2_header_names_the_client_and_skips_tlvs() -> Result {
        let mut payload = vec![203, 0, 113, 7, 10, 0, 0, 5];
        payload.extend_from_slice(&51234u16.to_be_bytes());
        payload.extend_from_slice(&1935u16.to_be_bytes());
        // An AWS VPC endpoint TLV, which is ignored.
        payload.extend_from_slice(&[0xea, 0x00, 0x03, 0x01, b'v', b'p']);
        let mut bytes = v2(0x1, 0x11, &payload);
        bytes.extend_from_slice(b"\x03");
        let (address, rest) = read(&bytes).await?;
        assert_eq!(address, "203.0.113.7:51234".parse()?);
        assert_eq!(rest, b"\x03");

        let mut payload = Ipv6Addr::LOCALHOST.octets().to_vec();
        payload.extend_from_slice(&Ipv6Addr::UNSPECIFIED.octets());
        payload.extend_from_slice(&[0x01, 0xbb, 0x07, 0x8f]);
        let (address, _) = read(&v2(0x1, 0x21, &payload)).await?;
        assert_eq!(address, "[::1]:443".parse()?);
        Ok(())
    }

    #[tokio::test]
    async fn a_v2_local_command_keeps_the_socket_peer() -> Result {
        let (address, rest) = read(&[v2(0x0, 0x00, &[]), b"x".to_vec()].concat()).await?;
        assert_eq!(address, peer());
        assert_eq!(rest, b"x");
        Ok(())
    }

    #[tokio::test]
    async fn anything_but_a_well_formed_header_is_refused() {
        let refused: [&[u8]; 7] = [
            // A client talking RTMP directly, bypassing the proxy.
            b"\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00rest",
            b"PROXY TCP4 203.0.113.7 10.0.0.5 51234\r\n",
            b"PROXY TCP4 not-an-address 10.0.0.5 1 2\r\n",
            b"PROXY UDP4 203.0.113.7 10.0.0.5 1 2\r\n",
            &[b"PROXY TCP4 ".as_slice(), &[b'1'; 120]].concat(),
            &v2(0x1, 0x11, &[203, 0, 113]),
            &v2(0x1, 0x12, &[0; 12]),
        ];
        for bytes in refused {
            assert!(
                read(bytes).await.is_err(),
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        let mut oversized = V2_SIGNATURE.to_vec();
        oversized.extend_from_slice(&[0x21, 0x11, 0xff, 0xff]);
        assert!(
            read(&oversized).await.is_err(),
            "the length is bounded before reading"
        );
    }
}
