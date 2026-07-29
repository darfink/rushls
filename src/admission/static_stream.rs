use subtle::ConstantTimeEq;

use crate::domain::BoxFuture;

use super::{AdmissionError, Authenticator, PublishGrant, PublishRequest};

/// One statically configured publisher and the grant its credential selects.
#[derive(Clone, derive_more::Debug)]
pub struct StaticPublisher {
    #[debug(skip)]
    credential: Vec<u8>,
    grant: PublishGrant,
}

impl StaticPublisher {
    pub fn new(credential: impl Into<Vec<u8>>, grant: PublishGrant) -> Self {
        Self {
            credential: credential.into(),
            grant,
        }
    }
}

/// Maps a configured set of credentials to their streams and policies.
///
/// A linear scan keeps credential comparison independent of secret-derived
/// hashing and is appropriate for the deliberately operator-managed list this
/// authenticator represents. Larger or dynamic publisher databases belong
/// behind a different [`Authenticator`] implementation.
#[derive(Clone, derive_more::Debug)]
#[debug("StaticStreamAuthenticator {{ publishers: {} }}", publishers.len())]
pub struct StaticStreamAuthenticator {
    #[debug(skip)]
    publishers: Vec<StaticPublisher>,
}

impl StaticStreamAuthenticator {
    pub fn new(publishers: Vec<StaticPublisher>) -> Self {
        Self { publishers }
    }

    pub fn len(&self) -> usize {
        self.publishers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.publishers.is_empty()
    }
}

impl Authenticator for StaticStreamAuthenticator {
    fn authenticate<'a>(
        &'a self,
        request: &'a PublishRequest,
    ) -> BoxFuture<'a, Result<PublishGrant, AdmissionError>> {
        Box::pin(async move {
            let mut grant = None;
            for publisher in &self.publishers {
                if publisher
                    .credential
                    .as_slice()
                    .ct_eq(request.credential.expose())
                    .into()
                {
                    grant = Some(publisher.grant.clone());
                }
            }
            grant.ok_or(AdmissionError::InvalidCredential)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        admission::{
            AdmissionError, Authenticator, Principal, PublishGrant, StreamPolicy, fixtures,
        },
        domain::StreamId,
    };

    use super::{StaticPublisher, StaticStreamAuthenticator};

    fn publisher(credential: &str, stream: &str) -> StaticPublisher {
        StaticPublisher::new(
            credential,
            PublishGrant {
                stream_id: StreamId::new(stream),
                principal: Principal(format!("{stream}-publisher")),
                policy: StreamPolicy::permissive(),
            },
        )
    }

    fn authenticator() -> StaticStreamAuthenticator {
        StaticStreamAuthenticator::new(vec![
            publisher("camera-secret", "live/camera"),
            publisher("stage-secret", "live/stage"),
        ])
    }

    #[tokio::test]
    async fn maps_each_key_to_its_configured_stream() {
        let grant = authenticator()
            .authenticate(&fixtures::publish_request("stage-secret"))
            .await
            .expect("credential matches");

        assert_eq!(grant.stream_id, StreamId::new("live/stage"));
        assert_eq!(grant.principal, Principal("live/stage-publisher".into()));
    }

    #[tokio::test]
    async fn rejects_an_unknown_credential() {
        assert!(matches!(
            authenticator()
                .authenticate(&fixtures::publish_request("wrong"))
                .await,
            Err(AdmissionError::InvalidCredential)
        ));
    }

    #[test]
    fn an_empty_authenticator_is_an_explicit_deny_all() {
        let authenticator = StaticStreamAuthenticator::new(Vec::new());

        assert!(authenticator.is_empty());
        assert_eq!(authenticator.len(), 0);
    }
}
