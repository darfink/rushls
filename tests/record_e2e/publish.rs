//! Owned MPEG-TS publisher: the whole file is in memory up front, so the
//! session demuxes as fast as it can. That is the "no pacing" burst.

use std::io::Cursor;
use std::sync::Arc;

use rushls::admission::{ClientInfo, IngestProtocol, PresentedCredential, PublishRequest, PublishResource};
use rushls::domain::BoxFuture;
use rushls::observe::SourceMeters;
use rushls::source::{
    AcceptedPublish, InputLimits, InputState, MpegTsConfig, MpegTsPacketSource, PendingPublish,
    PublishRejection, ReadInput, TransportError,
};

pub fn live_camera() -> PublishRequest {
    PublishRequest {
        protocol: IngestProtocol::Srt,
        resource: PublishResource { namespace: Some("live".into()), name: "camera".into() },
        credential: PresentedCredential::new("secret"),
        client: ClientInfo {
            remote_address: "127.0.0.1:1935".parse().expect("constant is valid"),
            encoder: Some("record-e2e".into()),
            protocol_version: None,
        },
    }
}

pub struct BurstMpegTs {
    pub request: PublishRequest,
    pub bytes: Vec<u8>,
}

impl PendingPublish for BurstMpegTs {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(self.request.clone())
    }

    fn accept(
        self: Box<Self>,
        grant: rushls::admission::PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(async move {
            // ReadInput::closed + Cursor<Vec<u8>>: the demux worker never waits
            // on the network, it just consumes. No throttle, no floor.
            let input = Box::new(ReadInput::closed(Cursor::new(self.bytes)));
            let source = MpegTsPacketSource::new(
                input,
                MpegTsConfig::default(),
                InputLimits::permissive(),
                meters,
            )
            .map_err(|e| TransportError::Accept(e.to_string().into()))?;
            // Enqueue-then-finish happens inside run_session's pipeline; the
            // only pacing left is how fast mux/record drain, which is what we
            // want to measure. Close the input only at EOF (ReadInput::closed
            // already models that: the cursor ends, the source ends).
            let _ = InputState::Closed;
            Ok(AcceptedPublish { source: Box::new(source), grant })
        })
    }

    fn reject(self: Box<Self>, _rejection: PublishRejection) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(async { Ok(()) })
    }
}

