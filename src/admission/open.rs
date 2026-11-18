use crate::domain::BoxFuture;

use super::{AdmissionError, Authenticator, Principal, PublishGrant, PublishRequest, StreamPolicy};

/// Admits every publisher and preserves its requested resource as the stream.
///
/// This authenticator is intentionally distinct from static authentication:
/// callers receive no identity guarantee, regardless of whether a transport
/// happens to present a credential.
#[derive(Clone, derive_more::Debug)]
pub struct OpenStreamAuthenticator {
    policy: StreamPolicy,
}

impl OpenStreamAuthenticator {
    pub fn new(policy: StreamPolicy) -> Self {
        Self { policy }
    }
}

impl Authenticator for OpenStreamAuthenticator {
    fn authenticate<'a>(
        &'a self,
        request: &'a PublishRequest,
    ) -> BoxFuture<'a, Result<PublishGrant, AdmissionError>> {
        Box::pin(async move {
            let stream_id = request
                .resource
                .stream_id()
                .ok_or(AdmissionError::Forbidden)?;
            Ok(PublishGrant {
                stream_id,
                principal: Principal("anonymous".into()),
                policy: self.policy.clone(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::{AdmissionError, Authenticator, Principal, StreamPolicy, fixtures},
        domain::StreamId,
    };

    use super::OpenStreamAuthenticator;

    #[tokio::test]
    async fn preserves_the_requested_resource() {
        let authenticator = OpenStreamAuthenticator::new(StreamPolicy::permissive());
        let request = fixtures::publish_request("presented-key");
        let grant = authenticator
            .authenticate(&request)
            .await
            .expect("a valid resource is admitted");

        assert_eq!(grant.stream_id, StreamId::new("live/presented-key"));
        assert_eq!(grant.principal, Principal("anonymous".into()));
        let publisher = request.into_publisher_context(&grant);
        assert!(!publisher.public_stream);
        assert_eq!(publisher.resource.name, "[redacted]");
        let accepted = crate::observe::SessionEvent::Accepted {
            stream: grant.stream_id,
            principal: grant.principal.0,
            publisher,
        };
        let projector = crate::observe::lifecycle::Projector::new();
        let session = crate::domain::SessionId(nz::u64!(1));
        assert!(projector.project(session, &accepted).is_none());
        assert!(
            projector
                .project(
                    session,
                    &crate::observe::SessionEvent::Ended {
                        end: crate::observe::SessionEnd::Ended,
                    }
                )
                .is_none()
        );
    }

    #[tokio::test]
    async fn preserves_a_resource_without_a_namespace() {
        let authenticator = OpenStreamAuthenticator::new(StreamPolicy::permissive());
        let mut request = fixtures::publish_request("ignored");
        request.resource.namespace = None;
        let grant = authenticator
            .authenticate(&request)
            .await
            .expect("a valid resource is admitted");

        assert_eq!(grant.stream_id, StreamId::new("presented-key"));
    }

    #[tokio::test]
    async fn rejects_an_empty_resource() {
        let authenticator = OpenStreamAuthenticator::new(StreamPolicy::permissive());
        let mut request = fixtures::publish_request("ignored");
        request.resource.name.clear();

        assert!(matches!(
            authenticator.authenticate(&request).await,
            Err(AdmissionError::Forbidden)
        ));
    }
}
