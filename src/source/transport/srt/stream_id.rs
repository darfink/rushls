use std::collections::HashSet;

use crate::admission::{PresentedCredential, PublishResource};

pub struct ParsedStreamId {
    pub resource: PublishResource,
    pub credential: PresentedCredential,
}

/// Parses the compact publishing form, the SRT access-control convention, or
/// a legacy free-form key.
///
/// The operator-facing form is `publish:<resource>[:<credential>]`. Omitting
/// the credential preserves the single-key convention where the resource also
/// acts as the credential. In the structured form, `u` is the presented
/// authorization identity and `r` is the publication resource.
pub fn parse(stream_id: &str, maximum_bytes: usize) -> Result<ParsedStreamId, Box<str>> {
    if stream_id.is_empty() {
        return Err("the SRT Stream ID is empty".into());
    }
    if stream_id.len() > maximum_bytes {
        return Err(format!(
            "the SRT Stream ID is {} bytes, above the permitted {maximum_bytes}",
            stream_id.len()
        )
        .into_boxed_str());
    }

    if let Some(fields) = stream_id.strip_prefix("publish:") {
        parse_compact(fields)
    } else if stream_id.starts_with("read:") || stream_id.starts_with("request:") {
        Err("the SRT Stream ID does not request publish mode".into())
    } else if let Some(fields) = stream_id.strip_prefix("#!::") {
        parse_structured(fields)
    } else if stream_id.starts_with("#!:") {
        Err("the SRT Stream ID uses an unsupported structured syntax".into())
    } else {
        Ok(ParsedStreamId {
            resource: PublishResource {
                namespace: None,
                name: stream_id.to_owned(),
            },
            credential: PresentedCredential::new(stream_id.as_bytes()),
        })
    }
}

fn parse_compact(fields: &str) -> Result<ParsedStreamId, Box<str>> {
    let (resource, credential) = fields
        .split_once(':')
        .map_or((fields, fields), |(resource, credential)| {
            (resource, credential)
        });
    if resource.is_empty() {
        return Err("the compact SRT Stream ID has no resource".into());
    }
    if credential.is_empty() {
        return Err("the compact SRT Stream ID has no credential".into());
    }

    Ok(ParsedStreamId {
        resource: split_resource(resource)?,
        // Keep the remainder opaque: authenticators may intentionally use a
        // `user:password` pair or another colon-bearing credential.
        credential: PresentedCredential::new(credential.as_bytes()),
    })
}

fn parse_structured(fields: &str) -> Result<ParsedStreamId, Box<str>> {
    let mut seen = HashSet::new();
    let mut user = None;
    let mut resource = None;
    let mut mode = None;
    let mut media_type = None;

    for field in fields.split(',') {
        let (key, value) = field
            .split_once('=')
            .filter(|(key, value)| !key.is_empty() && !value.is_empty())
            .ok_or_else(|| {
                Box::<str>::from("the structured SRT Stream ID contains an invalid field")
            })?;
        if !seen.insert(key) {
            return Err(format!("the structured SRT Stream ID repeats `{key}`").into_boxed_str());
        }
        match key {
            "u" => user = Some(value),
            "r" => resource = Some(value),
            "m" => mode = Some(value),
            "t" => media_type = Some(value),
            "h" | "s" => {}
            // Single-letter keys are reserved by the access-control
            // convention; silently treating a future standard key as custom
            // would give it application-specific semantics.
            key if key.len() == 1 => {
                return Err(
                    format!("the structured SRT Stream ID uses reserved key `{key}`")
                        .into_boxed_str(),
                );
            }
            _ => {}
        }
    }

    if mode.is_some_and(|mode| mode != "publish") {
        return Err("the SRT Stream ID does not request publish mode".into());
    }
    if media_type.is_some_and(|media_type| media_type != "stream") {
        return Err("the SRT Stream ID does not describe a live stream".into());
    }
    let user =
        user.ok_or_else(|| Box::<str>::from("the structured SRT Stream ID has no `u` field"))?;
    let resource = resource
        .ok_or_else(|| Box::<str>::from("the structured SRT Stream ID has no `r` field"))?;

    Ok(ParsedStreamId {
        resource: split_resource(resource)?,
        credential: PresentedCredential::new(user.as_bytes()),
    })
}

fn split_resource(resource: &str) -> Result<PublishResource, Box<str>> {
    match resource.rsplit_once('/') {
        Some((namespace, name)) if !namespace.is_empty() && !name.is_empty() => {
            Ok(PublishResource {
                namespace: Some(namespace.to_owned()),
                name: name.to_owned(),
            })
        }
        Some(_) => Err("the SRT resource contains an empty path component".into()),
        None => Ok(PublishResource {
            namespace: None,
            name: resource.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_form_ids_remain_single_key_credentials() {
        let parsed = parse("secret", 512).expect("free-form IDs are accepted");

        assert_eq!(parsed.resource.namespace, None);
        assert_eq!(parsed.resource.name, "secret");
        assert_eq!(parsed.credential.expose(), b"secret");
    }

    #[test]
    fn compact_ids_are_the_operator_facing_publish_form() {
        let parsed = parse("publish:live/camera:secret", 512).expect("compact IDs are accepted");

        assert_eq!(parsed.resource.namespace.as_deref(), Some("live"));
        assert_eq!(parsed.resource.name, "camera");
        assert_eq!(parsed.credential.expose(), b"secret");
    }

    #[test]
    fn compact_ids_support_single_keys_and_opaque_credentials() {
        let single_key = parse("publish:camera", 512).expect("credential can be implicit");
        assert_eq!(single_key.resource.name, "camera");
        assert_eq!(single_key.credential.expose(), b"camera");

        let user_password =
            parse("publish:camera:user:password", 512).expect("credential remains opaque");
        assert_eq!(user_password.resource.name, "camera");
        assert_eq!(user_password.credential.expose(), b"user:password");
    }

    #[test]
    fn structured_ids_separate_resource_and_authorization_identity() {
        let parsed = parse("#!::u=secret,r=live/camera,m=publish,t=stream", 512)
            .expect("standard fields are accepted");

        assert_eq!(parsed.resource.namespace.as_deref(), Some("live"));
        assert_eq!(parsed.resource.name, "camera");
        assert_eq!(parsed.credential.expose(), b"secret");
    }

    #[test]
    fn malformed_or_non_publishing_ids_are_rejected() {
        for stream_id in [
            "",
            "#!::u=secret,r=live/camera,m=request",
            "#!::u=secret,r=live/camera,t=file",
            "#!::u=secret,u=again,r=camera",
            "#!::u=secret",
            "#!::r=camera",
            "#!::u=secret,r=/camera",
            "#!::u=secret,r=camera,x",
            "publish:",
            "publish::secret",
            "publish:camera:",
            "read:camera",
            "request:camera",
        ] {
            assert!(parse(stream_id, 512).is_err(), "{stream_id:?}");
        }
    }
}
