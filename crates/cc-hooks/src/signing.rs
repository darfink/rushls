//! Standard Webhooks signing over the exact transmitted bytes.

use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderMap, HeaderValue};
use ring::hmac;

#[derive(Clone, derive_more::Debug)]
#[debug("SigningSecret([REDACTED])")]
pub struct SigningSecret(#[debug(skip)] hmac::Key);

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("a signing secret must be whsec_ followed by base64 encoding of at least 32 random bytes")]
pub struct InvalidSigningSecret;

impl SigningSecret {
    /// Decode a Standard Webhooks key. No credential material appears in errors.
    pub fn parse(value: &str) -> Result<Self, InvalidSigningSecret> {
        let bytes = STANDARD
            .decode(value.strip_prefix("whsec_").ok_or(InvalidSigningSecret)?)
            .map_err(|_| InvalidSigningSecret)?;
        if bytes.len() < 32 {
            return Err(InvalidSigningSecret);
        }
        Ok(Self(hmac::Key::new(hmac::HMAC_SHA256, &bytes)))
    }

    pub fn headers(&self, id: &str, timestamp: i64, body: &[u8]) -> HeaderMap {
        let timestamp = timestamp.to_string();
        let mut context = hmac::Context::with_key(&self.0);
        for part in [id.as_bytes(), b".", timestamp.as_bytes(), b".", body] {
            context.update(part);
        }
        let signature = format!("v1,{}", STANDARD.encode(context.sign().as_ref()));
        let mut headers = HeaderMap::new();
        headers.insert(
            "webhook-id",
            HeaderValue::from_str(id).expect("rendered event IDs are UUIDs"),
        );
        headers.insert(
            "webhook-timestamp",
            HeaderValue::from_str(&timestamp).expect("integer timestamp"),
        );
        headers.insert(
            "webhook-signature",
            HeaderValue::from_str(&signature).expect("base64 signature"),
        );
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_exact_bytes_and_redacts_the_key() -> Result<(), Box<dyn std::error::Error>> {
        let secret = SigningSecret::parse(&format!("whsec_{}", STANDARD.encode([0x0b; 32])))?;
        let headers = secret.headers("event-1", 1700000000, b"{\"x\":1}");
        // Independently generated with Python's hmac/hashlib, including separators.
        assert_eq!(
            headers["webhook-signature"],
            "v1,9DvJSHdiZ8rvAljEp90tkqqoZuZGuN9vAzXBdsOP7p0="
        );
        assert_ne!(
            headers,
            secret.headers("event-1", 1700000000, b"{ \"x\":1}")
        );
        assert_ne!(headers, secret.headers("event-2", 1700000000, b"{\"x\":1}"));
        assert_ne!(headers, secret.headers("event-1", 1700000001, b"{\"x\":1}"));
        assert_eq!(format!("{secret:?}"), "SigningSecret([REDACTED])");
        assert!(SigningSecret::parse("whsec_YQ==").is_err());
        assert!(SigningSecret::parse("secret").is_err());
        Ok(())
    }
}
