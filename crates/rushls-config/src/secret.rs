use crate::ConfigError;
use std::{fmt, path::Path, str::FromStr};

/// Configuration credentials stay redacted even when the whole schema is logged.
#[derive(Clone)]
pub struct SecretString(String);

/// Serde type errors normally quote the rejected value. Secret fields must
/// replace that diagnostic before the configuration parser sees it.
pub fn deserialize_secret<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map_err(|_| serde::de::Error::custom("invalid secret value"))
}

impl<'de> serde::Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_secret(deserializer).map(Self)
    }
}

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }
}
impl std::ops::Deref for SecretString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl FromStr for SecretString {
    type Err = std::convert::Infallible;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_owned()))
    }
}

/// Reads an optional inline or mounted credential, refusing ambiguous sources.
///
/// Mounted text commonly ends with a newline from a shell or secret projection.
/// Remove trailing line endings only: spaces can be part of a real credential.
/// Inline values remain untouched.
pub fn resolve_optional_text_secret(
    label: &str,
    inline: Option<&str>,
    file: Option<&Path>,
) -> Result<Option<String>, ConfigError> {
    match (inline, file) {
        (Some(_), Some(_)) => Err(ConfigError::SecretConflict(label.to_owned())),
        (Some(value), None) => Ok(Some(value.to_owned())),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(|value| Some(value.trim_end_matches(['\r', '\n']).to_owned()))
            .map_err(|source| ConfigError::SecretRead {
                secret: label.to_owned(),
                path: path.to_owned(),
                source,
            }),
        (None, None) => Ok(None),
    }
}
