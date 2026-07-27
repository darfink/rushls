//! Syntax-level HLS manifest rendering.
//!
//! These writers know how individual tags are spelled and how their attribute
//! lists are encoded. They deliberately do not decide which tags belong in a
//! playlist or whether their ordering is semantically valid; the playlist
//! projection above this module owns those decisions.

use std::{
    fmt::{self, Write},
    time::Duration,
};

use thiserror::Error;

mod media;
mod multivariant;

pub use media::{
    MediaPlaylistWriter, Part, PlaylistPresentationType, PreloadHint, PreloadHintType,
    RenditionReport, Segment, ServerControl,
};
pub use multivariant::{
    MultivariantPlaylistWriter, PlaylistMediaType, Rendition, Variant, VideoRange,
};

#[derive(Debug, Error)]
pub enum ManifestWriteError {
    #[error("the output rejected manifest text")]
    Output(#[from] fmt::Error),
    #[error("{field} contains a character HLS cannot represent in a quoted string")]
    InvalidQuotedString { field: &'static str },
    #[error("{field} is not a safely serializable resource URI")]
    InvalidUri { field: &'static str },
    #[error("{field} must not be zero")]
    ZeroDuration { field: &'static str },
    #[error("EXT-X-SERVER-CONTROL must contain at least one attribute")]
    EmptyServerControl,
    #[error("the program date-time could not be formatted as RFC 3339")]
    ProgramDateTime(#[from] time::error::Format),
}

pub type ManifestWriteResult<T> = Result<T, ManifestWriteError>;

pub fn validate_quoted(value: &str, field: &'static str) -> ManifestWriteResult<()> {
    if value.chars().any(|character| {
        character == '"' || character == '\r' || character == '\n' || character.is_control()
    }) {
        return Err(ManifestWriteError::InvalidQuotedString { field });
    }
    Ok(())
}

pub fn validate_uri(value: &str, field: &'static str) -> ManifestWriteResult<()> {
    if value.is_empty()
        || value.chars().any(|character| {
            character == '"' || character.is_whitespace() || character.is_control()
        })
    {
        return Err(ManifestWriteError::InvalidUri { field });
    }
    Ok(())
}

pub struct AttributeList<'a, W: Write + ?Sized> {
    out: &'a mut W,
    first: bool,
}

impl<'a, W: Write + ?Sized> AttributeList<'a, W> {
    pub fn new(out: &'a mut W) -> Self {
        Self { out, first: true }
    }

    pub fn item(&mut self, write: impl FnOnce(&mut W) -> fmt::Result) -> ManifestWriteResult<()> {
        if !self.first {
            self.out.write_char(',')?;
        }
        self.first = false;
        write(self.out)?;
        Ok(())
    }
}

pub struct DecimalSeconds(pub Duration);

impl fmt::Display for DecimalSeconds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        let mut fraction = self.0.subsec_nanos();
        if fraction == 0 {
            return write!(formatter, "{seconds}");
        }

        let mut digits = 9;
        while fraction.is_multiple_of(10) {
            fraction /= 10;
            digits -= 1;
        }
        write!(
            formatter,
            "{seconds}.{fraction:0width$}",
            width = digits as usize
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_are_rendered_exactly_without_floating_point() {
        assert_eq!(
            DecimalSeconds(Duration::from_micros(333_340)).to_string(),
            "0.33334"
        );
        assert_eq!(
            DecimalSeconds(Duration::from_millis(2_500)).to_string(),
            "2.5"
        );
        assert_eq!(DecimalSeconds(Duration::from_secs(4)).to_string(), "4");
    }

    #[test]
    fn quoted_values_reject_manifest_injection() {
        assert!(matches!(
            validate_quoted("English\"\n#EXT-X-ENDLIST", "NAME"),
            Err(ManifestWriteError::InvalidQuotedString { field: "NAME" })
        ));
    }

    #[test]
    fn resource_uris_must_already_be_encoded() {
        assert!(matches!(
            validate_uri("parts/next part.m4s", "URI"),
            Err(ManifestWriteError::InvalidUri { field: "URI" })
        ));
    }
}
