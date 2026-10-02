//! Environment substitution in TOML string values, before source overrides.
//! Keys are never interpolated. Inserted values are literal, not TOML source.

use std::{collections::BTreeMap, ffi::OsString};

use thiserror::Error;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InterpolationError {
    #[error("unclosed environment reference; a variable is written as ${{NAME}}")]
    Unclosed,
    #[error("${{{0}}} names no variable, and no `:-` fallback was given")]
    Undefined(String),
    #[error("invalid environment variable name in interpolation")]
    Malformed,
}

/// The environment a document is interpolated against.
pub struct Environment(BTreeMap<String, String>);

impl Environment {
    pub fn new(variables: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self(
            variables
                .into_iter()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
        )
    }

    /// Substitutes every string leaf in `document`, in place.
    ///
    /// Keys and table names are never touched: a variable that could name a
    /// setting rather than a value would make the shape of the configuration
    /// depend on the environment, which is not something an operator reading
    /// the file could follow.
    pub fn interpolate(&self, document: &mut toml::Value) -> Result<(), InterpolationError> {
        match document {
            toml::Value::String(value) => {
                *value = self.expand(value)?;
                Ok(())
            }
            toml::Value::Array(values) => values
                .iter_mut()
                .try_for_each(|value| self.interpolate(value)),
            toml::Value::Table(table) => table
                .iter_mut()
                .try_for_each(|(_, value)| self.interpolate(value)),
            _ => Ok(()),
        }
    }

    fn expand(&self, value: &str) -> Result<String, InterpolationError> {
        let mut out = String::with_capacity(value.len());
        let mut rest = value;

        while let Some(index) = rest.find('$') {
            out.push_str(&rest[..index]);
            rest = &rest[index..];

            // `$$` is a literal dollar. This matters rather than being
            // pedantry: passphrases and stream keys legitimately contain one,
            // and silently interpolating those would corrupt a working
            // credential.
            if let Some(tail) = rest.strip_prefix("$$") {
                out.push('$');
                rest = tail;
                continue;
            }

            let Some(tail) = rest.strip_prefix("${") else {
                // A lone `$` that opens nothing is itself. Refusing here would
                // make every dollar in a password an error.
                out.push('$');
                rest = &rest[1..];
                continue;
            };
            let Some(end) = tail.find('}') else {
                return Err(InterpolationError::Unclosed);
            };
            out.push_str(&self.lookup(&tail[..end])?);
            rest = &tail[end + 1..];
        }

        out.push_str(rest);
        Ok(out)
    }

    /// Resolves one `NAME` or `NAME:-fallback` reference.
    fn lookup(&self, reference: &str) -> Result<String, InterpolationError> {
        let (name, fallback) = match reference.split_once(":-") {
            Some((name, fallback)) => (name, Some(fallback)),
            None => (reference, None),
        };
        let name = name.trim();
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(InterpolationError::Malformed);
        }

        match self.0.get(name) {
            // `:-` means what it means in a shell: unset *or empty* takes the
            // fallback. Orchestrators often define a variable as empty rather
            // than leaving it out, and that should not silently win.
            Some(value) if value.is_empty() && fallback.is_some() => {
                Ok(fallback.unwrap_or_default().to_owned())
            }
            Some(value) => Ok(value.clone()),
            // An undefined variable is an error rather than an empty string.
            // Substituting `""` into a token would silently disable the check
            // it was protecting, which is the worst way to learn a variable was
            // missing. Where empty is genuinely wanted, `${VAR:-}` says so.
            None => fallback
                .map(ToOwned::to_owned)
                .ok_or_else(|| InterpolationError::Undefined(name.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> Environment {
        Environment::new(
            pairs
                .iter()
                .map(|(key, value)| (OsString::from(*key), OsString::from(*value))),
        )
    }

    fn expand(value: &str, pairs: &[(&str, &str)]) -> Result<String, InterpolationError> {
        environment(pairs).expand(value)
    }

    #[test]
    fn a_variable_is_replaced_by_its_value() {
        assert_eq!(
            expand("${NAME}", &[("NAME", "studio")]),
            Ok("studio".into())
        );
    }

    #[test]
    fn a_value_may_be_composed_from_several_variables_and_literal_text() {
        // The reason for the braces: without them there is no way to say where
        // a name ends and the text after it begins.
        assert_eq!(
            expand(
                "https://${HOST}:${PORT}/admit",
                &[("HOST", "auth.internal"), ("PORT", "8081")]
            ),
            Ok("https://auth.internal:8081/admit".into())
        );
    }

    #[test]
    fn an_undefined_variable_is_an_error_rather_than_an_empty_string() {
        assert_eq!(
            expand("${MISSING}", &[]),
            Err(InterpolationError::Undefined("MISSING".into())),
            "substituting nothing would silently disable the check the value \
             was protecting"
        );
    }

    #[test]
    fn a_fallback_makes_an_absent_variable_deliberate() {
        assert_eq!(expand("${MISSING:-local}", &[]), Ok("local".into()));
        assert_eq!(
            expand("${MISSING:-}", &[]),
            Ok(String::new()),
            "an empty fallback is how an operator asks for empty on purpose"
        );
        assert_eq!(
            expand("${NAME:-local}", &[("NAME", "studio")]),
            Ok("studio".into()),
            "a set variable wins over its fallback"
        );
        assert_eq!(
            expand("${NAME:-local}", &[("NAME", "")]),
            Ok("local".into()),
            "as in a shell, an empty variable takes the fallback too"
        );
        assert_eq!(
            expand("${NAME}", &[("NAME", "")]),
            Ok(String::new()),
            "without a fallback, a deliberately empty variable stays empty"
        );
    }

    #[test]
    fn a_literal_dollar_is_escaped_and_survives_intact() {
        // A passphrase may legitimately contain a dollar, and interpolating it
        // would corrupt a working credential.
        assert_eq!(expand("p$$ssw0rd", &[]), Ok("p$ssw0rd".into()));
        assert_eq!(expand("$$${NAME}", &[("NAME", "x")]), Ok("$x".into()));
    }

    #[test]
    fn a_dollar_that_opens_nothing_is_itself() {
        assert_eq!(expand("100$ per hour", &[]), Ok("100$ per hour".into()));
    }

    #[test]
    fn an_unclosed_reference_is_refused() {
        assert!(matches!(
            expand("${NAME", &[("NAME", "x")]),
            Err(InterpolationError::Unclosed)
        ));
    }

    #[test]
    fn a_record_pattern_passes_through_untouched() {
        // `record.path` uses bare braces and is expanded per segment by a
        // different layer. The sigils are distinct so that one cannot eat the
        // other.
        let pattern = "{stream}/{time:%Y/%m/%d}/{rendition}_{segment}.m4s";
        assert_eq!(expand(pattern, &[]), Ok(pattern.to_owned()));
    }

    #[test]
    fn only_string_leaves_are_substituted() {
        let mut document: toml::Value = toml::from_str(
            r#"
name = "${NAME}"
shutdown_grace = "10s"
streams = 256

[publish]
codecs = ["${CODEC}", "aac"]
"#,
        )
        .expect("the fixture parses");

        environment(&[("NAME", "studio"), ("CODEC", "opus")])
            .interpolate(&mut document)
            .expect("every reference resolves");

        assert_eq!(document["name"].as_str(), Some("studio"));
        assert_eq!(document["shutdown_grace"].as_str(), Some("10s"));
        assert_eq!(document["streams"].as_integer(), Some(256));
        assert_eq!(document["publish"]["codecs"][0].as_str(), Some("opus"));
        assert_eq!(document["publish"]["codecs"][1].as_str(), Some("aac"));
    }
}
