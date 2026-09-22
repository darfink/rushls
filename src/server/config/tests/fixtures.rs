//! Reads active settings and commented examples from the annotated configuration example.

use std::error::Error;

pub struct Example {
    pub document: toml::Table,
    pub examples: Vec<(String, toml::Value)>,
}

impl Example {
    pub fn read() -> Result<Self, Box<dyn Error>> {
        let text = include_str!("../../../../rushls.example.toml");
        let mut reference = Self {
            document: toml::Table::new(),
            examples: Vec::new(),
        };
        let mut section = "";
        for line in text.lines() {
            let line = line.trim().strip_prefix('#').unwrap_or(line).trim();
            if let Some(header) = line.strip_prefix('[') {
                section = header.split_once(']').ok_or("unfinished table header")?.0;
                continue;
            }
            let Some((key, _)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            // Prose can contain equals signs. Only a bare TOML field begins an example.
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let fields: toml::Table = toml::from_str(line)?;
            for (key, value) in fields {
                let path = if section.is_empty() {
                    key
                } else {
                    format!("{section}.{key}")
                };
                set(&mut reference.document, &path, value.clone())?;
                reference.examples.push((path, value));
            }
        }
        Ok(reference)
    }
}

pub fn set(
    document: &mut toml::Table,
    path: &str,
    value: toml::Value,
) -> Result<(), Box<dyn Error>> {
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
            .ok_or("reference path passes through a scalar")?;
    }
    Err("empty reference path".into())
}

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
