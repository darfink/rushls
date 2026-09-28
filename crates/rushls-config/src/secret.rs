use crate::ConfigError;
use std::{fmt, path::PathBuf, str::FromStr};

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

/// Text written inline or read from a file: `"value"` or `{ file = "/path" }`.
///
/// One field per credential, rather than an inline field and a `_file` twin
/// that must not both be set: the value's shape makes the choice, so it can
/// never be ambiguous. Environment values use the same two spellings; a value
/// that parses as `{ file = "..." }` names a file, anything else is literal.
#[derive(Clone)]
pub enum TextSource {
    Inline(SecretString),
    File(PathBuf),
}

impl TextSource {
    /// Reads the text. Mounted files commonly end with a newline from a shell
    /// or secret projection, so trailing CR/LF is removed from files only;
    /// spaces can be part of a real credential and inline values are untouched.
    pub fn read(&self, label: &str) -> Result<String, ConfigError> {
        match self {
            Self::Inline(value) => Ok(value.to_string()),
            Self::File(path) => std::fs::read_to_string(path)
                .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
                .map_err(|source| ConfigError::SecretRead {
                    secret: label.to_owned(),
                    path: path.clone(),
                    source,
                }),
        }
    }
}

/// The table form, kept strict so a typo such as `{ path = ... }` is refused
/// rather than read as some other credential.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReference {
    file: PathBuf,
}

impl<'de> serde::Deserialize<'de> for TextSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Inline(String),
            File(FileReference),
        }
        // Redacted like every secret: the rejected value may be the credential.
        match Shape::deserialize(deserializer)
            .map_err(|_| serde::de::Error::custom("expected a string or { file = \"/path\" }"))?
        {
            Shape::Inline(value) => Ok(Self::Inline(SecretString(value))),
            Shape::File(reference) => Ok(Self::File(reference.file)),
        }
    }
}

impl FromStr for TextSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim_start().starts_with('{') {
            #[derive(serde::Deserialize)]
            struct Wrapper {
                value: FileReference,
            }
            return toml::from_str::<Wrapper>(&format!("value = {value}"))
                .map(|wrapper| Self::File(wrapper.value.file))
                .map_err(|_| "expected { file = \"/path\" }".to_owned());
        }
        Ok(Self::Inline(SecretString(value.to_owned())))
    }
}

impl fmt::Debug for TextSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline(_) => f.write_str("Inline([REDACTED])"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
        }
    }
}
