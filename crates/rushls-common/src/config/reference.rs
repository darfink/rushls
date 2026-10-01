//! Configuration reference generated from the schema itself.
//!
//! A hand-written reference drifts: a default changes in code and nobody
//! edits the table. Rendering it from `conf`'s introspection means it can only
//! describe settings that exist, with the defaults they really have, and an
//! application test that compares the rendering with the committed file fails
//! when either moves alone.

use std::fmt::Write as _;

use conf::{Conf, introspection::ProgramOptionMeta};

/// How one application's schema maps onto its reference document.
pub struct Reference<'a> {
    /// Everything above the first section: title, regeneration instructions,
    /// and conventions.
    pub preamble: &'a str,
    /// The TOML path of a schema option ID, or `None` for an option that is
    /// not a TOML setting (command-line-only flags, credential file flags).
    pub toml_path: &'a dyn Fn(&str) -> Option<String>,
    /// Credential file flags, as TOML-style paths (`metrics.token_file`).
    ///
    /// `conf` refuses a flag on a secret field, so each credential with a
    /// flag has a CLI-only `<name>_file` sibling; the reference shows that
    /// flag on the credential's own row.
    pub credential_file_flags: &'a [&'a str],
    /// Whole-table settings `conf` treats as one opaque value, with the keys
    /// each accepts, so the reference can still list them.
    pub tables: &'a [(&'a str, &'a [TableKey<'a>])],
}

/// One key inside a whole-table setting.
pub struct TableKey<'a> {
    /// Appended to the table's path, e.g. `.<name>.url`.
    pub suffix: &'a str,
    /// Empty when there is no default.
    pub default: &'a str,
    pub description: &'a str,
}

/// Renders the reference: one Markdown table per top-level section, in schema
/// order. The description is each field's first doc paragraph; rationale
/// below it stays in the source.
pub fn reference_markdown<T: Conf>(reference: &Reference<'_>) -> String {
    let cell = |text: String| text.replace('|', "\\|");
    let code =
        |value: Option<String>| value.map_or_else(|| "—".to_owned(), |value| format!("`{value}`"));
    let mut output = reference.preamble.to_owned();
    let mut section = None;
    for option in T::program_options() {
        let Some(path) = (reference.toml_path)(&option.id().to_string()) else {
            continue;
        };
        let table_keys = reference
            .tables
            .iter()
            .find(|(table, _)| *table == path)
            .map(|(_, keys)| *keys);
        let top = match path.split_once('.') {
            Some((top, _)) => top.to_owned(),
            // Whole-table settings head their own section.
            None if table_keys.is_some() => path.clone(),
            None => "(top level)".to_owned(),
        };
        if section.as_ref() != Some(&top) {
            write!(
                output,
                "\n## {}\n\n| Setting | Default | Environment | CLI | Description |\n|---|---|---|---|---|\n",
                if top == "(top level)" {
                    top.clone()
                } else {
                    format!("[{top}]")
                }
            )
            .expect("writing to a String cannot fail");
            section = Some(top);
        }
        let description = option
            .description()
            .map(ToString::to_string)
            .unwrap_or_default();
        let summary = description
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let flag = option
            .long_form()
            .map(|value| format!("--{value}"))
            .or_else(|| {
                let sibling = format!("{path}_file");
                reference
                    .credential_file_flags
                    .contains(&sibling.as_str())
                    .then(|| format!("--{} <PATH>", path.replace(['.', '_'], "-")))
            });
        writeln!(
            output,
            "| `{path}` | {} | {} | {} | {} |",
            cell(code(option.default_help_str().map(ToString::to_string))),
            cell(code(option.env_form().map(ToString::to_string))),
            cell(code(flag)),
            cell(summary),
        )
        .expect("writing to a String cannot fail");
        for key in table_keys.unwrap_or_default() {
            writeln!(
                output,
                "| `{path}{}` | {} | — | — | {} |",
                key.suffix,
                cell(if key.default.is_empty() {
                    "—".to_owned()
                } else {
                    format!("`{}`", key.default)
                }),
                cell(key.description.to_owned()),
            )
            .expect("writing to a String cannot fail");
        }
    }
    output
}
