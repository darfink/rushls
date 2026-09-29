//! Transport-observed publisher identity, shared by admission and lifecycle events.

use super::StreamId;
use derive_more::Display;
use std::net::SocketAddr;

// The display form is the wire vocabulary (rtmp, srt, moq), the same spelling
// operators see in the publisher API and in serialized events. It is
// deliberately not the Rust variant name: a log line reading Rtmp while the
// JSON body says "rtmp" is one protocol with two spellings.
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq, serde::Serialize)]
#[display(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum IngestProtocol {
    Rtmp,
    /// RTMP inside TLS, terminated by this node.
    ///
    /// Named apart from `rtmp` so an admission service can require
    /// encryption; RTMP behind an external terminator still reports `rtmp`,
    /// because nothing here can tell that the hop before it was encrypted.
    Rtmps,
    Srt,
    Moq,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PublishResource {
    pub namespace: Option<String>,
    pub name: String,
}

impl PublishResource {
    /// Preserves the transport's requested resource as a canonical stream ID.
    ///
    /// The namespace may itself contain separators (for example an SRT
    /// resource such as `organization/live/camera`), so joining happens only
    /// at this already-normalized resource boundary.
    pub fn stream_id(&self) -> Option<StreamId> {
        if self.name.is_empty() {
            return None;
        }

        match &self.namespace {
            Some(namespace) if namespace.is_empty() => None,
            Some(namespace) => Some(StreamId::new(format!("{namespace}/{}", self.name))),
            None => Some(StreamId::new(self.name.clone())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ClientInfo {
    pub remote_address: SocketAddr,
    pub encoder: Option<String>,
    pub protocol_version: Option<String>,
}

/// Facts observed before authentication. Credentials are deliberately excluded.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PublisherContext {
    pub protocol: IngestProtocol,
    pub resource: PublishResource,
    pub client: ClientInfo,
}

#[cfg(test)]
mod tests {
    use super::IngestProtocol;

    #[test]
    fn the_display_form_matches_the_wire_vocabulary() {
        // A log line and the JSON body beside it must not disagree about how a
        // protocol is spelled.
        for (protocol, wire) in [
            (IngestProtocol::Rtmp, "rtmp"),
            (IngestProtocol::Rtmps, "rtmps"),
            (IngestProtocol::Srt, "srt"),
            (IngestProtocol::Moq, "moq"),
        ] {
            assert_eq!(protocol.to_string(), wire);
            assert_eq!(serde_json::to_value(protocol).expect("serializes"), wire);
        }
    }
}
