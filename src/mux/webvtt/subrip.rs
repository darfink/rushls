//! Safe conversion of one plain-text SubRip cue into WebVTT cue text.

use std::{str, sync::Arc};

use super::{MuxError, mux_error, normalize_newlines};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InlineTag {
    Bold,
    Font,
    Italic,
    Underline,
}

pub fn convert(bytes: &[u8]) -> Result<Arc<str>, MuxError> {
    let text = str::from_utf8(bytes).map_err(|_| mux_error("SubRip cue text is not UTF-8"))?;
    if text.contains('\0') {
        return Err(mux_error("SubRip cue text contains a NUL byte"));
    }
    let normalized = normalize_newlines(text);
    if normalized.contains("{\\") {
        return Err(mux_error("SubRip override codes are not supported"));
    }

    let mut output = String::with_capacity(normalized.len());
    let mut tags = Vec::new();
    let mut cursor = 0;
    while cursor < normalized.len() {
        let rest = &normalized[cursor..];
        if rest.starts_with('<') {
            let Some(relative_end) = rest.find('>') else {
                push_escaped_text(&mut output, "<");
                cursor += 1;
                continue;
            };
            let end = cursor + relative_end;
            let tag = &normalized[cursor + 1..end];
            let tag_name = tag
                .trim()
                .strip_prefix('/')
                .unwrap_or(tag.trim())
                .trim_start();
            if !tag_name.starts_with(|value: char| value.is_ascii_alphabetic()) {
                push_escaped_text(&mut output, &normalized[cursor..=end]);
                cursor = end + 1;
                continue;
            }
            convert_tag(tag, &mut tags, &mut output)?;
            cursor = end + 1;
            continue;
        }
        if rest.starts_with('&') {
            if let Some((consumed, value)) = decode_entity(rest)? {
                push_escaped_char(&mut output, value);
                cursor += consumed;
            } else {
                output.push_str("&amp;");
                cursor += 1;
            }
            continue;
        }

        let next = rest
            .char_indices()
            .find_map(|(index, value)| matches!(value, '<' | '&').then_some(index))
            .unwrap_or(rest.len());
        push_escaped_text(&mut output, &rest[..next]);
        cursor += next;
    }
    if !tags.is_empty() {
        return Err(mux_error("SubRip cue contains unclosed formatting tags"));
    }
    if output.is_empty() || output.contains("\n\n") {
        return Err(mux_error(
            "SubRip cue text is empty or contains a blank line",
        ));
    }
    Ok(Arc::from(output))
}

fn convert_tag(raw: &str, stack: &mut Vec<InlineTag>, output: &mut String) -> Result<(), MuxError> {
    let raw = raw.trim();
    let (closing, body) = raw
        .strip_prefix('/')
        .map_or((false, raw), |body| (true, body.trim_start()));
    let mut fields = body.split_whitespace();
    let name = fields.next().unwrap_or_default().to_ascii_lowercase();
    let has_attributes = fields.next().is_some();
    let tag = match name.as_str() {
        "b" if !has_attributes => InlineTag::Bold,
        "i" if !has_attributes => InlineTag::Italic,
        "u" if !has_attributes => InlineTag::Underline,
        "font" => InlineTag::Font,
        _ => {
            return Err(mux_error(format!(
                "SubRip cue contains unsupported <{raw}> markup"
            )));
        }
    };
    if closing {
        if has_attributes || stack.pop() != Some(tag) {
            return Err(mux_error("SubRip cue formatting tags are not balanced"));
        }
    } else {
        stack.push(tag);
    }
    match (tag, closing) {
        (InlineTag::Bold, false) => output.push_str("<b>"),
        (InlineTag::Bold, true) => output.push_str("</b>"),
        (InlineTag::Italic, false) => output.push_str("<i>"),
        (InlineTag::Italic, true) => output.push_str("</i>"),
        (InlineTag::Underline, false) => output.push_str("<u>"),
        (InlineTag::Underline, true) => output.push_str("</u>"),
        (InlineTag::Font, _) => {}
    }
    Ok(())
}

fn decode_entity(input: &str) -> Result<Option<(usize, char)>, MuxError> {
    let Some(end) = input.get(1..).and_then(|rest| rest.find(';')) else {
        return Ok(None);
    };
    let consumed = end + 2;
    let entity = &input[1..consumed - 1];
    let value = match entity {
        "amp" => '&',
        "apos" => '\'',
        "gt" => '>',
        "lt" => '<',
        "nbsp" => '\u{00a0}',
        "quot" => '"',
        numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
            u32::from_str_radix(&numeric[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| mux_error("SubRip cue contains an invalid numeric entity"))?
        }
        numeric if numeric.starts_with('#') => numeric[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| mux_error("SubRip cue contains an invalid numeric entity"))?,
        _ => return Ok(None),
    };
    if value == '\0' {
        return Err(mux_error("SubRip cue entity resolves to a NUL byte"));
    }
    Ok(Some((consumed, value)))
}

fn push_escaped_text(output: &mut String, text: &str) {
    for value in text.chars() {
        push_escaped_char(output, value);
    }
}

fn push_escaped_char(output: &mut String, value: char) {
    match value {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        value => output.push(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_styles_and_entities_become_webvtt_cue_text() -> Result<(), MuxError> {
        let converted =
            convert(b"<B>Hello &amp; <font color=\"#f00\"><i>world</i></font></B>\r\nSecond line")?;

        assert_eq!(
            converted.as_ref(),
            "<b>Hello &amp; <i>world</i></b>\nSecond line"
        );
        Ok(())
    }

    #[test]
    fn angle_brackets_in_ordinary_text_are_escaped() -> Result<(), MuxError> {
        let converted = convert(b"1 < 5 > 3")?;

        assert_eq!(converted.as_ref(), "1 &lt; 5 &gt; 3");
        Ok(())
    }

    #[test]
    fn unbalanced_and_unsupported_markup_is_rejected() {
        assert!(convert(b"<b>broken").is_err());
        assert!(convert(b"<ruby>unsupported</ruby>").is_err());
        assert!(convert(br"{\an8}positioned").is_err());
    }
}
