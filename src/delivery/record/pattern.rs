//! Compile placeholders once, expand only at segment completion.
use chrono::{
    DateTime, Utc,
    format::{Item, StrftimeItems},
};
use std::{path::PathBuf, time::SystemTime};

#[derive(Clone, Debug)]
pub struct Pattern(Vec<Token>);
#[derive(Clone, Debug)]
enum Token {
    Literal(String),
    Stream,
    Publication,
    Rendition,
    Segment,
    Time(String),
}
impl Pattern {
    pub fn parse(mut value: &str) -> Result<Self, String> {
        let mut tokens = Vec::new();
        while !value.is_empty() {
            let at = value.find('{').unwrap_or(value.len());
            let literal = &value[..at];
            if literal.contains('}') {
                return Err("unmatched closing brace in record.path".into());
            }
            tokens.push(Token::Literal(literal.into()));
            value = &value[at..];
            if value.is_empty() {
                break;
            }
            let end = value
                .find('}')
                .ok_or("unclosed placeholder in record.path")?;
            let name = &value[1..end];
            tokens.push(match name {
                "stream" => Token::Stream,
                "publication" => Token::Publication,
                "rendition" => Token::Rendition,
                "segment" => Token::Segment,
                _ => {
                    let format = name
                        .strip_prefix("time:")
                        .ok_or_else(|| format!("unknown recording placeholder {{{name}}}"))?;
                    if format.is_empty()
                        || StrftimeItems::new(format).any(|item| matches!(item, Item::Error))
                    {
                        return Err("invalid strftime format in record.path".into());
                    }
                    Token::Time(format.into())
                }
            });
            value = &value[end + 1..];
        }
        let pattern = Self(tokens);
        pattern.expand(
            "stream",
            "publication",
            "rendition",
            0,
            SystemTime::UNIX_EPOCH,
            false,
        )?;
        pattern.refuse_erased_segment()?;
        Ok(pattern)
    }
    /// Refuses a pattern whose `{segment}` the subtitle suffix would erase.
    ///
    /// [`Self::expand`] replaces the final suffix with `.vtt` for a text
    /// rendition, which drops everything after the last dot of the last path
    /// component. A pattern such as `{rendition}.{segment}` therefore names one
    /// file for every subtitle segment: the first is written and every later
    /// one is refused as a collision, which the operator only finds when the
    /// archive turns out to be missing subtitles.
    ///
    /// The check expands twice with different segment numbers rather than once,
    /// because a single expansion of a pattern like that succeeds: nothing is
    /// wrong with the path itself, only with the two paths being the same one.
    ///
    /// Only a pattern that uses `{segment}` is checked. Omitting a placeholder
    /// is supported — the writer never overwrites, so a pattern that collides
    /// on some axis the pattern cannot see, such as two streams sharing a
    /// `{rendition}_{segment}` name, still fails at the file rather than here.
    fn refuse_erased_segment(&self) -> Result<(), String> {
        if !self.0.iter().any(|token| matches!(token, Token::Segment)) {
            return Ok(());
        }
        let text = |segment| {
            self.expand(
                "stream",
                "publication",
                "rendition",
                segment,
                SystemTime::UNIX_EPOCH,
                true,
            )
        };
        if text(0)? == text(1)? {
            return Err(
                "record.path cannot leave {segment} at the end of a component: the .vtt suffix \
                 of a subtitle rendition replaces everything after the last dot, so every subtitle \
                 segment would name the same file; move {segment} before the final suffix"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn expand(
        &self,
        stream: &str,
        publication: &str,
        rendition: &str,
        segment: u64,
        time: SystemTime,
        text: bool,
    ) -> Result<PathBuf, String> {
        // Validate substitutions even when a custom pattern omits them.
        safe(stream)?;
        safe(publication)?;
        safe(rendition)?;
        let time: DateTime<Utc> = time.into();
        let mut path = String::new();
        for token in &self.0 {
            match token {
                Token::Literal(value) => path.push_str(value),
                Token::Stream => path.push_str(stream),
                Token::Publication => path.push_str(publication),
                Token::Rendition => path.push_str(rendition),
                Token::Segment => path.push_str(&segment.to_string()),
                Token::Time(format) => path.push_str(&time.format(format).to_string()),
            }
        }
        safe(&path)?;
        let mut path = PathBuf::from(path);
        if text {
            path.set_extension("vtt");
        }
        Ok(path)
    }
}
fn safe(value: &str) -> Result<(), String> {
    if value.contains(['\\', '\0'])
        || value.chars().any(char::is_control)
        || value.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.starts_with(".rushls-")
        })
    {
        return Err("recording names must be relative paths without empty, dot, parent, or reserved components".into());
    }
    Ok(())
}
