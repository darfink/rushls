//! One loading contract for application-owned `conf::Conf` schemas.
//!
//! Values resolve as defaults < TOML < environment < CLI. Loading never changes
//! the process environment or constructs application services. Schema derives
//! supply field names, parsing, help, and secret-aware diagnostics.

use conf::{
    Conf, ConfSerde,
    introspection::{ConfigEvent, ValueSource},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
};
use thiserror::Error;

pub mod example;
pub mod interpolate;
pub mod reference;
mod secret;
mod values;
pub use bytesize::ByteSize;
pub use humantime::parse_duration;
pub use secret::{SecretString, TextSource, deserialize_secret};
pub use values::{
    OptionalAddress, OptionalBytes, OptionalDuration, TomlTable, TomlValue, parse_optional_address,
    parse_optional_bytes, parse_optional_duration, read_credential,
};

/// Tests use explicit paths so developer files cannot change their defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigSearch {
    ExplicitOnly,
    WellKnown,
}

/// Metadata only: configuration values, including credentials, are never kept.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
    Default,
    Cli,
    Environment(String),
    File(String),
    Other,
}

pub struct Loaded<T> {
    pub config: T,
    pub path: Option<PathBuf>,
    pub sources: BTreeMap<String, Source>,
    /// Nonfatal diagnostics, containing variable names but never their values.
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    // Parser source text can contain credentials. Keep syntax diagnostics but
    // never display the source line, including through Debug or error chains.
    #[error("invalid TOML in {path} at byte {offset}: {message}")]
    Toml {
        path: PathBuf,
        offset: usize,
        message: String,
    },
    #[error("could not interpolate {path}: {source}")]
    Interpolation {
        path: PathBuf,
        source: interpolate::InterpolationError,
    },
    #[error(transparent)]
    Sources(#[from] conf::Error),
    #[error("could not read {secret} from {path}: {source}")]
    SecretRead {
        secret: String,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl ConfigError {
    /// Help and version use exit status zero. Startup errors use status two.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Sources(error) => error.exit_code(),
            _ => 2,
        }
    }
}

/// The application selects its namespace and schema; the loader owns sources.
#[derive(Clone, Copy, Debug)]
pub struct Loader {
    pub name: &'static str,
    pub env_prefix: &'static str,
    pub search: ConfigSearch,
}

impl Loader {
    pub const fn new(name: &'static str, env_prefix: &'static str) -> Self {
        Self {
            name,
            env_prefix,
            search: ConfigSearch::WellKnown,
        }
    }

    pub const fn explicit_only(mut self) -> Self {
        self.search = ConfigSearch::ExplicitOnly;
        self
    }

    pub fn load<T: ConfSerde>(&self) -> Result<Loaded<T>, ConfigError> {
        self.load_from(std::env::args_os(), std::env::vars_os())
    }

    /// Uses a snapshot so parsing and interpolation see exactly the same inputs.
    pub fn load_from<T: ConfSerde>(
        &self,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        env: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Result<Loaded<T>, ConfigError> {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let env: Vec<(OsString, OsString)> =
            env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        // Help remains usable even when the configured file or environment is
        // broken. The actual parser decides whether a token is a help request.
        if args
            .iter()
            .skip(1)
            .take_while(|arg| *arg != "--")
            .any(|arg| matches!(arg.to_str(), Some("--help" | "-h" | "--version" | "-V")))
            && let Err(error) = T::conf_builder()
                .args(args.clone())
                .env(Vec::<(OsString, OsString)>::new())
                .try_parse()
            && error.exit_code() == 0
        {
            return Err(error.into());
        }
        let warnings = self.environment_warnings::<T>(&env);
        let config_env = format!("{}CONFIG", self.env_prefix);
        let path = conf::find_parameter("config", args.iter().cloned())
            .map(PathBuf::from)
            .or_else(|| {
                env.iter()
                    .rev()
                    .find(|(key, _)| key == config_env.as_str())
                    .map(|(_, value)| PathBuf::from(value))
            })
            .or_else(|| match self.search {
                ConfigSearch::ExplicitOnly => None,
                ConfigSearch::WellKnown => {
                    self.config_paths().into_iter().find(|path| path.is_file())
                }
            });
        let mut sources = BTreeMap::new();
        let record = |event: &dyn ConfigEvent| {
            let source = match event.value_source() {
                ValueSource::Args { .. } => Source::Cli,
                ValueSource::Env { var, .. } => Source::Environment(var.to_string()),
                ValueSource::Document { name, .. } => Source::File(name.to_string()),
                ValueSource::Default { .. } => Source::Default,
                _ => Source::Other,
            };
            sources.insert(event.program_option().id().to_string(), source);
        };
        let builder = T::conf_builder()
            .args(args)
            .env(env.clone())
            .config_logger(record);
        let config = match &path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                let mut document: toml::Value =
                    toml::from_str(&text).map_err(|error: toml::de::Error| ConfigError::Toml {
                        path: path.clone(),
                        offset: error.span().map_or(0, |span| span.start),
                        // Generic syntax text can include a duplicate key chosen by
                        // the input. Values and whole source lines are not retained.
                        message: error.message().to_owned(),
                    })?;
                interpolate::Environment::new(env)
                    .interpolate(&mut document)
                    .map_err(|source| ConfigError::Interpolation {
                        path: path.clone(),
                        source,
                    })?;
                builder
                    .doc(path.display().to_string(), document)
                    .try_parse()?
            }
            None => builder.try_parse()?,
        };
        Ok(Loaded {
            config,
            path,
            sources,
            warnings,
        })
    }

    /// Warns about unknown names in this application's namespace, in stable order.
    pub fn environment_warnings<T: Conf>(&self, env: &[(OsString, OsString)]) -> Vec<String> {
        // The public introspection view omits aliases. Use the same generated
        // option metadata as conf's parser so accepted aliases do not produce warnings.
        let mut known = BTreeSet::new();
        for option in T::PROGRAM_OPTIONS.iter() {
            known.extend(
                option
                    .env_form
                    .iter()
                    .chain(option.env_aliases.iter())
                    .map(ToString::to_string),
            );
        }
        let unknown: BTreeSet<String> = env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .filter(|key| key.starts_with(self.env_prefix) && !known.contains(key))
            .collect();
        unknown.into_iter()
            .map(|name| format!("unrecognized environment variable {name} is ignored as a configuration override"))
            .collect()
    }

    /// Discovery order is working directory, local config, roaming config,
    /// then, on Unix, the system-wide `/etc/<name>/<name>.toml` where a
    /// packaged service conventionally keeps it.
    pub fn config_paths(&self) -> Vec<PathBuf> {
        let filename = format!("{}.toml", self.name);
        let mut paths = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            paths.push(cwd.join(&filename));
        }
        if let Some(dirs) = directories::ProjectDirs::from("", "", self.name) {
            for directory in [dirs.config_local_dir(), dirs.config_dir()] {
                let path = directory.join(&filename);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        if cfg!(unix) {
            paths.push(Path::new("/etc").join(self.name).join(&filename));
        }
        paths
    }
}

/// The same byte-size spelling for environment variables and CLI arguments.
pub fn parse_bytes(value: &str) -> Result<usize, String> {
    let bytes: ByteSize = value.parse()?;
    usize::try_from(bytes.as_u64())
        .map_err(|_| "byte size exceeds this platform's address space".into())
}

/// TOML can use an integer byte count or the human-readable CLI spelling.
pub fn deserialize_bytes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<usize, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Bytes {
        Count(usize),
        Text(String),
    }
    match <Bytes as serde::Deserialize>::deserialize(deserializer)? {
        Bytes::Count(value) => Ok(value),
        Bytes::Text(value) => parse_bytes(&value).map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests;
