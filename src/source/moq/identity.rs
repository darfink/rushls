//! WebTransport URL and moq-lite SETUP path → publication identity.
//!
//! Same resource spelling as SRT: the last `/` splits namespace from name, and a
//! single path component is a single-key publish. The credential is the `token`
//! query parameter when present; otherwise the resource name, matching the SRT
//! convention that one key identifies both the stream and the publisher.
//!
//! The query name is spelled here rather than imported from HLS. Ingest must
//! not depend on delivery, even when the two surfaces happen to share a word.

use url::Url;

use crate::admission::{PresentedCredential, PublishResource};
use crate::source::TransportError;

/// Query key carrying the presented publish credential.
pub const CREDENTIAL_QUERYPARAM: &str = "token";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishIdentity {
    pub resource: PublishResource,
    pub credential: PresentedCredential,
}

/// What the WebTransport CONNECT URL contributed, before SETUP is considered.
///
/// An empty path is not a failure: moq-lite can still name the resource in
/// SETUP, and that is the fallback when a client dials the origin root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialIdentity {
    pub resource: Option<PublishResource>,
    pub credential: Option<PresentedCredential>,
}

impl PartialIdentity {
    /// Completes identity from the moq-lite SETUP path when the CONNECT URL
    /// did not name a resource.
    pub fn complete_with_setup(self, setup_path: &str) -> Result<PublishIdentity, TransportError> {
        let resource = match self.resource {
            Some(resource) => resource,
            None => split_resource(&decode_setup_path(setup_path)?)?,
        };
        let credential = match self.credential {
            Some(credential) => credential,
            None => PresentedCredential::new(resource.name.as_bytes()),
        };
        Ok(PublishIdentity {
            resource,
            credential,
        })
    }
}

pub fn from_webtransport_url(url: &Url) -> Result<PartialIdentity, TransportError> {
    let resource = path_resource(url)?;
    let credential = query_credential(url);
    let credential = match (credential, &resource) {
        (Some(credential), _) => Some(credential),
        (None, Some(resource)) => Some(PresentedCredential::new(resource.name.as_bytes())),
        (None, None) => None,
    };
    Ok(PartialIdentity {
        resource,
        credential,
    })
}

fn path_resource(url: &Url) -> Result<Option<PublishResource>, TransportError> {
    // `//` in the path is an empty component, not a collapsed namespace. Url's
    // segment iterator would skip it and silently rename the resource.
    if url.path().contains("//") {
        return Err(invalid("the publication path contains an empty component"));
    }
    let mut segments = Vec::new();
    let Some(parts) = url.path_segments() else {
        return Err(invalid(
            "the WebTransport URL path is not a valid publication resource",
        ));
    };
    for segment in parts {
        if segment.is_empty() {
            continue;
        }
        let decoded = decode_percent(segment)?;
        if decoded.contains('/') || decoded.contains('\\') {
            return Err(invalid(
                "a publication path segment contains an encoded separator",
            ));
        }
        segments.push(decoded);
    }
    if segments.is_empty() {
        return Ok(None);
    }
    Ok(Some(split_owned_segments(segments)?))
}

fn query_credential(url: &Url) -> Option<PresentedCredential> {
    url.query_pairs()
        .find(|(key, value)| key == CREDENTIAL_QUERYPARAM && !value.is_empty())
        .map(|(_, value)| PresentedCredential::new(value.as_bytes()))
}

fn decode_setup_path(path: &str) -> Result<String, TransportError> {
    let trimmed = path.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(invalid("the publication path is empty"));
    }
    decode_percent(trimmed)
}

/// Path segments, not form fields: `+` is a character, not a space.
fn decode_percent(segment: &str) -> Result<String, TransportError> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3).ok_or_else(|| {
                invalid("the publication path is not valid percent-encoded UTF-8")
            })?;
            let hex = std::str::from_utf8(hex)
                .map_err(|_| invalid("the publication path is not valid percent-encoded UTF-8"))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| invalid("the publication path is not valid percent-encoded UTF-8"))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| invalid("the publication path is not valid percent-encoded UTF-8"))
}

fn split_resource(resource: &str) -> Result<PublishResource, TransportError> {
    let segments: Vec<String> = resource
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.is_empty() {
        return Err(invalid("the publication path is empty"));
    }
    if resource.split('/').any(str::is_empty) {
        return Err(invalid("the publication path contains an empty component"));
    }
    split_owned_segments(segments)
}

fn split_owned_segments(mut segments: Vec<String>) -> Result<PublishResource, TransportError> {
    let name = segments
        .pop()
        .ok_or_else(|| invalid("the publication path is empty"))?;
    if name.is_empty() {
        return Err(invalid("the publication path contains an empty component"));
    }
    if segments.is_empty() {
        return Ok(PublishResource {
            namespace: None,
            name,
        });
    }
    if segments.iter().any(String::is_empty) {
        return Err(invalid("the publication path contains an empty component"));
    }
    Ok(PublishResource {
        namespace: Some(segments.join("/")),
        name,
    })
}

fn invalid(message: impl Into<Box<str>>) -> TransportError {
    TransportError::InvalidPublishRequest(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(path_and_query: &str) -> Result<PartialIdentity, TransportError> {
        from_webtransport_url(
            &Url::parse(&format!("https://ingest.example{path_and_query}")).expect("fixture URL"),
        )
    }

    fn complete(path_and_query: &str) -> Result<PublishIdentity, TransportError> {
        parse(path_and_query)?.complete_with_setup("")
    }

    #[test]
    fn a_namespaced_path_and_token_are_the_operator_facing_form() -> Result<(), TransportError> {
        let identity = complete("/live/camera?token=secret")?;
        assert_eq!(identity.resource.namespace.as_deref(), Some("live"));
        assert_eq!(identity.resource.name, "camera");
        assert_eq!(identity.credential.expose(), b"secret");
        Ok(())
    }

    #[test]
    fn a_single_path_component_is_a_single_key_credential() -> Result<(), TransportError> {
        let identity = complete("/camera")?;
        assert_eq!(identity.resource.namespace, None);
        assert_eq!(identity.resource.name, "camera");
        assert_eq!(identity.credential.expose(), b"camera");
        Ok(())
    }

    #[test]
    fn an_empty_path_is_completed_from_setup() -> Result<(), TransportError> {
        let identity = parse("/")?.complete_with_setup("live/camera")?;
        assert_eq!(identity.resource.namespace.as_deref(), Some("live"));
        assert_eq!(identity.resource.name, "camera");
        assert_eq!(identity.credential.expose(), b"camera");
        Ok(())
    }

    #[test]
    fn a_token_on_an_empty_path_is_kept_when_setup_names_the_resource() -> Result<(), TransportError>
    {
        let identity = parse("/?token=secret")?.complete_with_setup("camera")?;
        assert_eq!(identity.resource.name, "camera");
        assert_eq!(identity.credential.expose(), b"secret");
        Ok(())
    }

    #[test]
    fn percent_encoded_path_segments_are_decoded() -> Result<(), TransportError> {
        let identity = complete("/live/caf%C3%A9")?;
        assert_eq!(identity.resource.name, "café");
        Ok(())
    }

    #[test]
    fn nested_namespaces_split_on_the_last_slash() -> Result<(), TransportError> {
        let identity = complete("/org/live/camera")?;
        assert_eq!(identity.resource.namespace.as_deref(), Some("org/live"));
        assert_eq!(identity.resource.name, "camera");
        Ok(())
    }

    #[test]
    fn an_empty_path_without_setup_is_invalid() {
        let identity = parse("/").expect("an empty CONNECT path is still a partial identity");
        assert!(identity.complete_with_setup("").is_err());
    }

    #[test]
    fn empty_path_components_are_rejected() {
        assert!(complete("/live//camera").is_err());
    }
    #[test]
    fn encoded_separators_cannot_change_the_resource_boundary() {
        assert!(complete("/live%2fcamera").is_err());
        assert!(complete("/live/camera%5cother").is_err());
    }
}
