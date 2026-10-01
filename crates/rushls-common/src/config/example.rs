//! Reads an annotated example configuration, commented settings included.
//!
//! An application ships one example that documents every TOML setting, with
//! optional ones commented out. Its tests use this to check that the example
//! names every schema option and that every documented value, active or
//! commented, still parses — so the file an operator copies cannot drift from
//! the binary that reads it.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExampleError {
    #[error("unfinished table header: {0}")]
    Header(String),
    #[error("example line {line:?} is not TOML: {source}")]
    Toml {
        line: String,
        source: toml::de::Error,
    },
    #[error("example path {0} passes through a scalar")]
    Scalar(String),
}

/// Every setting the example mentions, merged into one document.
pub struct AnnotatedExample {
    /// Active and commented settings together. Alternatives for one key
    /// overwrite each other, so this proves presence, not one valid file.
    pub document: toml::Table,
    /// Each documented `(path, value)`, in file order, to parse one at a time.
    pub examples: Vec<(String, toml::Value)>,
}

impl AnnotatedExample {
    /// A line is a setting when, after an optional `#`, it starts with a bare
    /// key and `=`; prose that merely contains `=` is skipped.
    pub fn parse(text: &str) -> Result<Self, ExampleError> {
        let mut example = Self {
            document: toml::Table::new(),
            examples: Vec::new(),
        };
        let mut section = "";
        for line in text.lines() {
            let line = line.trim().strip_prefix('#').unwrap_or(line).trim();
            if let Some(header) = line.strip_prefix('[') {
                section = header
                    .split_once(']')
                    .ok_or_else(|| ExampleError::Header(line.to_owned()))?
                    .0;
                continue;
            }
            let Some((key, _)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let fields: toml::Table =
                toml::from_str(line).map_err(|source| ExampleError::Toml {
                    line: line.to_owned(),
                    source,
                })?;
            for (key, value) in fields {
                let path = if section.is_empty() {
                    key
                } else {
                    format!("{section}.{key}")
                };
                set(&mut example.document, &path, value.clone())?;
                example.examples.push((path, value));
            }
        }
        Ok(example)
    }
}

/// Inserts `value` at a dotted `path`, creating intermediate tables.
pub fn set(document: &mut toml::Table, path: &str, value: toml::Value) -> Result<(), ExampleError> {
    let mut table = document;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            table.insert(part.to_owned(), value);
            return Ok(());
        }
        table = table
            .entry(part)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| ExampleError::Scalar(path.to_owned()))?;
    }
    Ok(())
}

/// Whether a dotted `path` exists in `document`.
pub fn contains(document: &toml::Table, path: &str) -> bool {
    let mut parts = path.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    let mut value = document.get(first);
    for part in parts {
        value = value.and_then(|value| value.get(part));
    }
    value.is_some()
}
