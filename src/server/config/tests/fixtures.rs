//! Reads active settings and commented examples from the annotated configuration example.

use std::error::Error;

pub use rushls_common::config::example::{AnnotatedExample as Example, contains, set};

/// The shipped example, parsed with its commented settings.
pub fn example() -> Result<Example, Box<dyn Error>> {
    Ok(Example::parse(include_str!(
        "../../../../rushls.example.toml"
    ))?)
}

/// Escape a path before inserting it inside a double-quoted TOML string.
/// JSON string escaping covers the escapes needed by these fixture paths.
pub fn toml_path_contents(path: &std::path::Path) -> String {
    let quoted = serde_json::to_string(&path.to_string_lossy()).expect("a string serializes");
    quoted[1..quoted.len() - 1].to_owned()
}
