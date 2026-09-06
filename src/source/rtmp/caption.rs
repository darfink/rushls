//! RTMP script-data captions (`onCaption` / `onTextData`).
//!
//! Encoders put cue text in an AMF0 property named `text`. That is a subtitle
//! track (`Codec::Text`), not in-band SEI: H.264 captions stay on the video
//! access units and are scanned separately.

use std::io::Cursor;

use rml_amf0::Amf0Value;

const ON_CAPTION: &str = "onCaption";
const ON_TEXT_DATA: &str = "onTextData";

/// UTF-8 cue body when `payload` is an `onCaption` or `onTextData` message.
///
/// Unknown script names, malformed AMF0, and objects without a string `text`
/// property are ignored: a caption glitch must not fail the publication.
pub fn cue_text(payload: &[u8]) -> Option<String> {
    let values = rml_amf0::deserialize(&mut Cursor::new(payload)).ok()?;
    let Amf0Value::Utf8String(name) = values.first()? else {
        return None;
    };
    if name != ON_CAPTION && name != ON_TEXT_DATA {
        return None;
    }
    let Amf0Value::Object(properties) = values.get(1)? else {
        return None;
    };
    let Amf0Value::Utf8String(text) = properties.get("text")? else {
        return None;
    };
    Some(text.clone())
}

/// AMF0 body matching what encoders emit: the message name, then an ECMA
/// array `{ text: ... }`.
#[cfg(test)]
pub fn encode_cue(name: &[u8], text: &[u8]) -> bytes::Bytes {
    let mut payload = Vec::with_capacity(16 + name.len() + text.len());
    payload.push(0x02);
    payload.extend_from_slice(&u16::try_from(name.len()).expect("name fits").to_be_bytes());
    payload.extend_from_slice(name);
    payload.push(0x08);
    payload.extend_from_slice(&1_u32.to_be_bytes());
    payload.extend_from_slice(&4_u16.to_be_bytes());
    payload.extend_from_slice(b"text");
    payload.push(0x02);
    payload.extend_from_slice(&u16::try_from(text.len()).expect("text fits").to_be_bytes());
    payload.extend_from_slice(text);
    payload.extend_from_slice(&[0x00, 0x00, 0x09]);
    bytes::Bytes::from(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_caption_and_on_text_data_yield_the_cue_body() {
        for name in [b"onCaption".as_slice(), b"onTextData".as_slice()] {
            let payload = encode_cue(name, b"hello");
            assert_eq!(cue_text(&payload).as_deref(), Some("hello"));
        }
    }

    #[test]
    fn unknown_script_names_are_ignored() {
        let payload = encode_cue(b"onCuePoint", b"hello");
        assert_eq!(cue_text(&payload), None);
    }

    #[test]
    fn empty_cue_text_is_a_clear() {
        let payload = encode_cue(b"onCaption", b"");
        assert_eq!(cue_text(&payload).as_deref(), Some(""));
    }
}
