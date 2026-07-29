use super::{ClientInfo, IngestProtocol, PresentedCredential, PublishRequest, PublishResource};

pub fn publish_request(credential: &str) -> PublishRequest {
    PublishRequest {
        protocol: IngestProtocol::Rtmp,
        resource: PublishResource {
            namespace: Some("live".into()),
            name: "presented-key".into(),
        },
        credential: PresentedCredential::new(credential),
        client: ClientInfo {
            remote_address: "127.0.0.1:1935".parse().expect("constant is valid"),
            encoder: None,
            protocol_version: None,
        },
    }
}
