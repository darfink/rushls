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
    MediaPlaylistWriter, Part, PreloadHint, PreloadHintType, RenditionReport, Segment,
    ServerControl,
};
pub use multivariant::{
    MultivariantPlaylistWriter, PlaylistMediaType, Rendition, Variant, VideoRange,
};

#[derive(Debug, Error)]
pub enum ManifestWriteError {
    #[error("the output rejected manifest text")]
    Output(#[from] fmt::Error),
    #[error("{tag} {attribute} contains a character HLS cannot represent in a quoted string")]
    InvalidQuotedString {
        tag: &'static str,
        attribute: &'static str,
    },
    #[error("{tag} {attribute} is not a safely serializable resource URI")]
    InvalidUri {
        tag: &'static str,
        attribute: &'static str,
    },
    #[error("{field} must not be zero")]
    ZeroDuration { field: &'static str },
    #[error("{tag} must contain at least one attribute")]
    EmptyAttributeList { tag: &'static str },
    #[error("the program date-time could not be formatted as RFC 3339")]
    ProgramDateTime(#[from] time::error::Format),
}

pub type ManifestWriteResult<T> = Result<T, ManifestWriteError>;

/// Refuses a value that could close its quote or the line holding it.
pub fn validate_quoted(
    value: &str,
    tag: &'static str,
    attribute: &'static str,
) -> ManifestWriteResult<()> {
    if value.chars().any(|character| {
        character == '"' || character == '\r' || character == '\n' || character.is_control()
    }) {
        return Err(ManifestWriteError::InvalidQuotedString { tag, attribute });
    }
    Ok(())
}

/// Refuses a resource name that is not already encoded.
pub fn validate_uri(
    value: &str,
    tag: &'static str,
    attribute: &'static str,
) -> ManifestWriteResult<()> {
    if value.is_empty()
        || value.chars().any(|character| {
            character == '"' || character.is_whitespace() || character.is_control()
        })
    {
        return Err(ManifestWriteError::InvalidUri { tag, attribute });
    }
    Ok(())
}

/// Writes one tag's comma-separated attribute list.
///
/// Every attribute goes through a method that knows how its value is encoded,
/// so quoting and validation cannot be applied to some values and forgotten on
/// others — which is the shape a manifest injection takes. Each value is
/// checked at the moment it is written rather than in a separate pass, so
/// adding an attribute cannot leave a validation list one entry behind.
///
/// A tag is written straight into the playlist, and a value refused partway
/// through rewinds the output to where the tag began. That keeps a half-written
/// tag from ever being observable without needing a buffer to assemble it in:
/// the playlist reserves its whole capacity up front, so these bytes land in
/// memory that is already there.
pub struct AttributeList<'a> {
    out: &'a mut String,
    /// Where this tag begins, which is how far back a refusal rewinds.
    start: usize,
    tag: &'static str,
    first: bool,
}

impl<'a> AttributeList<'a> {
    /// Opens `#{tag}:` and prepares to write its attributes.
    pub fn new(out: &'a mut String, tag: &'static str) -> Self {
        let start = out.len();
        out.push('#');
        out.push_str(tag);
        out.push(':');
        Self {
            out,
            start,
            tag,
            first: true,
        }
    }

    /// A value HLS represents without quoting.
    pub fn plain(
        &mut self,
        attribute: &str,
        value: impl fmt::Display,
    ) -> ManifestWriteResult<&mut Self> {
        self.separate();
        let _ = write!(self.out, "{attribute}={value}");
        Ok(self)
    }

    /// The same, written only when the value is present.
    pub fn optional(
        &mut self,
        attribute: &str,
        value: Option<impl fmt::Display>,
    ) -> ManifestWriteResult<&mut Self> {
        match value {
            Some(value) => self.plain(attribute, value),
            None => Ok(self),
        }
    }

    /// A quoted string, refused if it could close the quote or the line.
    pub fn quoted(
        &mut self,
        attribute: &'static str,
        value: &str,
    ) -> ManifestWriteResult<&mut Self> {
        if let Err(error) = validate_quoted(value, self.tag, attribute) {
            return Err(self.abandon(error));
        }
        self.separate();
        let _ = write!(self.out, r#"{attribute}="{value}""#);
        Ok(self)
    }

    /// The same, written only when the value is present.
    pub fn optional_quoted(
        &mut self,
        attribute: &'static str,
        value: Option<&str>,
    ) -> ManifestWriteResult<&mut Self> {
        match value {
            Some(value) => self.quoted(attribute, value),
            None => Ok(self),
        }
    }

    /// A quoted string naming a resource, which must already be encoded.
    pub fn uri(&mut self, attribute: &'static str, value: &str) -> ManifestWriteResult<&mut Self> {
        if let Err(error) = validate_uri(value, self.tag, attribute) {
            return Err(self.abandon(error));
        }
        self.separate();
        let _ = write!(self.out, r#"{attribute}="{value}""#);
        Ok(self)
    }

    /// `YES` or `NO`, for an attribute whose default is not what silence means.
    pub fn boolean(&mut self, attribute: &str, value: bool) -> ManifestWriteResult<&mut Self> {
        self.plain(attribute, if value { "YES" } else { "NO" })
    }

    /// `YES` when set and omitted otherwise, for the attributes whose absence
    /// already says `NO`.
    pub fn flag(&mut self, attribute: &str, value: bool) -> ManifestWriteResult<&mut Self> {
        if value {
            self.plain(attribute, "YES")?;
        }
        Ok(self)
    }

    /// Closes the tag, or refuses one that would carry no attributes.
    pub fn end(mut self) -> ManifestWriteResult<()> {
        if self.first {
            let error = ManifestWriteError::EmptyAttributeList { tag: self.tag };
            return Err(self.abandon(error));
        }
        self.out.push('\n');
        Ok(())
    }

    fn separate(&mut self) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
    }

    /// Unwrites the tag, so a refused value leaves no trace of it behind.
    fn abandon(&mut self, error: ManifestWriteError) -> ManifestWriteError {
        self.out.truncate(self.start);
        error
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

        let mut digits = 9_usize;
        while fraction.is_multiple_of(10) {
            fraction /= 10;
            digits -= 1;
        }
        write!(formatter, "{seconds}.{fraction:0digits$}")
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
            validate_quoted("English\"\n#EXT-X-ENDLIST", "EXT-X-MEDIA", "NAME"),
            Err(ManifestWriteError::InvalidQuotedString {
                tag: "EXT-X-MEDIA",
                attribute: "NAME"
            })
        ));
    }

    #[test]
    fn resource_uris_must_already_be_encoded() {
        assert!(matches!(
            validate_uri("parts/next part.m4s", "EXT-X-PART", "URI"),
            Err(ManifestWriteError::InvalidUri {
                tag: "EXT-X-PART",
                attribute: "URI"
            })
        ));
    }
}
