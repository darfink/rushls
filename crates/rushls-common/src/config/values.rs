//! Value shapes shared by application schemas.
//!
//! Each answers an operator question the plain Rust type cannot express: a
//! limit that can be lifted, a listener that can be turned off, or a
//! structured value that `conf` hands over as raw TOML text.

use std::{collections::BTreeMap, fmt, net::SocketAddr, path::Path, str::FromStr, time::Duration};

use bytesize::ByteSize;
use serde::Deserialize;

use crate::config::{ConfigError, TextSource};

/// A duration that may be explicitly turned off with `"off"` or `"none"`.
///
/// "No timeout" has to stay expressible: a trusted link on a controlled
/// network is a legitimate reason to wait indefinitely, and without `"off"` an
/// operator who wants that is pushed into writing an absurd number instead —
/// which reads as a mistake and behaves like one if it is ever reached.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OptionalDuration(pub Option<Duration>);

impl fmt::Display for OptionalDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(duration) => write!(formatter, "{}", humantime::format_duration(duration)),
            None => formatter.write_str("off"),
        }
    }
}

pub fn parse_optional_duration(value: &str) -> Result<OptionalDuration, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(OptionalDuration(None));
    }
    humantime::parse_duration(trimmed)
        .map(|duration| OptionalDuration(Some(duration)))
        .map_err(|error| error.to_string())
}

/// A byte limit that may be explicitly lifted with `"unlimited"`.
///
/// Like [`OptionalDuration`]'s "off": a trusted deployment may reasonably opt
/// out of enforcement, and spelling that as an absurd number reads as a
/// mistake. Usage is still accounted and reported either way.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OptionalBytes(pub Option<ByteSize>);

impl fmt::Display for OptionalBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(bytes) => write!(formatter, "{bytes}"),
            None => formatter.write_str("unlimited"),
        }
    }
}

pub fn parse_optional_bytes(value: &str) -> Result<OptionalBytes, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("unlimited") {
        return Ok(OptionalBytes(None));
    }
    trimmed.parse().map(|bytes| OptionalBytes(Some(bytes)))
}

/// A listener address that may be explicitly disabled with `"off"`.
///
/// Mirrors [`OptionalDuration`]: turning one listener off is how an operator
/// says "this protocol only", and an absurd address is not a way to spell that.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OptionalAddress(pub Option<SocketAddr>);

impl fmt::Display for OptionalAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(address) => write!(formatter, "{address}"),
            None => formatter.write_str("off"),
        }
    }
}

pub fn parse_optional_address(value: &str) -> Result<OptionalAddress, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("off") {
        return Ok(OptionalAddress(None));
    }
    trimmed
        .parse()
        .map(|address| OptionalAddress(Some(address)))
        .map_err(|error| format!("{error}"))
}

/// A TOML table of entries keyed by the name an operator chose.
///
/// `conf` hands a table-valued parameter over as its own TOML text rather than
/// as a parsed value, so each of these has to parse itself. Keyed by name so
/// uniqueness is structural: TOML rejects a duplicate key, where an array of
/// tables with a `name` field would need a validation rule.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct TomlTable<T>(pub BTreeMap<String, T>);

// Hand-written rather than derived: an empty table is meaningful for every `T`,
// and deriving would demand `T: Default` for no reason.
impl<T> Default for TomlTable<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<T: serde::de::DeserializeOwned> FromStr for TomlTable<T> {
    type Err = toml::de::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        toml::from_str(value).map_err(|mut error| {
            // Entries can contain credentials beside the invalid field.
            error.set_input(None);
            error
        })
    }
}

/// One TOML value handed over as its own text.
///
/// `conf` gives a table-valued parameter as raw TOML rather than as a parsed
/// value, so each of these parses itself. The same shape also lets an
/// environment override carry a TOML fragment, which keeps a structured value
/// overridable rather than file-only.
#[derive(Clone, Debug, Deserialize)]
#[serde(transparent)]
pub struct TomlValue<T>(pub T);

impl<T: serde::de::DeserializeOwned> FromStr for TomlValue<T> {
    type Err = toml::de::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        #[derive(Deserialize)]
        struct Wrapper<T> {
            value: T,
        }
        // Wrapped in a key so a bare inline table parses as a document, which
        // is the form both a file fragment and an environment value take.
        toml::from_str::<Wrapper<T>>(&format!("value = {value}")).map(|wrapper| Self(wrapper.value))
    }
}

/// Reads a credential that also has a command-line flag.
///
/// Arguments are visible to every local user through the process list, so
/// the flag only ever names a file; it is never the credential itself. It
/// sits at the CLI's place in the precedence order, above TOML and the
/// environment.
pub fn read_credential(
    label: &str,
    source: Option<&TextSource>,
    cli_file: Option<&Path>,
) -> Result<Option<String>, ConfigError> {
    match cli_file {
        Some(path) => TextSource::File(path.to_path_buf()).read(label).map(Some),
        None => source.map(|source| source.read(label)).transpose(),
    }
}
