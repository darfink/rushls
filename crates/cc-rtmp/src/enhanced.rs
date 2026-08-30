use std::collections::HashMap;

use rml_amf0::Amf0Value;

/// Enhanced RTMP structural validation policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnhancedValidationMode {
    /// Reject malformed Enhanced FLV structures and invalid capability fields.
    #[default]
    Strict,
    /// Keep malformed or unknown Enhanced media as opaque bytes.
    Passthrough,
}

impl std::str::FromStr for EnhancedValidationMode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "passthrough" => Ok(Self::Passthrough),
            _ => Err("validation mode must be strict or passthrough"),
        }
    }
}

/// Enhanced RTMP fields retained from an AMF0 `connect` object.
///
/// Unknown properties are retained separately so callers can inspect them,
/// but [`Self::forwardable_properties`] deliberately excludes connection-local
/// values such as `tcUrl` and `flashVer`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnhancedCapabilities {
    pub caps_ex: Option<Amf0Value>,
    pub four_cc_list: Option<Amf0Value>,
    pub video_four_cc_info_map: Option<Amf0Value>,
    pub audio_four_cc_info_map: Option<Amf0Value>,
    pub video_function: Option<Amf0Value>,
    pub audio_function: Option<Amf0Value>,
    pub unknown_properties: HashMap<String, Amf0Value>,
    /// Known capability fields retained opaquely in passthrough mode because
    /// their AMF shape was invalid.
    pub invalid_fields: Vec<String>,
}

impl EnhancedCapabilities {
    pub const FIELD_NAMES: [&'static str; 6] = [
        "fourCcList",
        "videoFourCcInfoMap",
        "audioFourCcInfoMap",
        "capsEx",
        "videoFunction",
        "audioFunction",
    ];

    /// Extract capability fields and retain all remaining properties.
    pub fn from_connect_properties(properties: &HashMap<String, Amf0Value>) -> Self {
        let mut value = Self {
            caps_ex: properties.get("capsEx").cloned(),
            four_cc_list: properties.get("fourCcList").cloned(),
            video_four_cc_info_map: properties.get("videoFourCcInfoMap").cloned(),
            audio_four_cc_info_map: properties.get("audioFourCcInfoMap").cloned(),
            video_function: properties.get("videoFunction").cloned(),
            audio_function: properties.get("audioFunction").cloned(),
            unknown_properties: properties.clone(),
            invalid_fields: Vec::new(),
        };
        for name in Self::FIELD_NAMES {
            value.unknown_properties.remove(name);
        }
        value
    }

    /// Extract and validate capability fields according to the session mode.
    pub fn parse(
        properties: &HashMap<String, Amf0Value>,
        mode: EnhancedValidationMode,
    ) -> Result<Self, String> {
        let mut value = Self::from_connect_properties(properties);
        for (name, valid) in [
            (
                "capsEx",
                value
                    .caps_ex
                    .as_ref()
                    .is_none_or(|value| valid_mask(value, 0x0f)),
            ),
            (
                "fourCcList",
                value.four_cc_list.as_ref().is_none_or(valid_four_cc_list),
            ),
            (
                "videoFourCcInfoMap",
                value
                    .video_four_cc_info_map
                    .as_ref()
                    .is_none_or(valid_four_cc_info_map),
            ),
            (
                "audioFourCcInfoMap",
                value
                    .audio_four_cc_info_map
                    .as_ref()
                    .is_none_or(valid_four_cc_info_map),
            ),
            (
                "videoFunction",
                value
                    .video_function
                    .as_ref()
                    .is_none_or(|value| valid_mask(value, 0x0f)),
            ),
            (
                "audioFunction",
                value.audio_function.as_ref().is_none_or(valid_u32),
            ),
        ] {
            if !valid {
                value.invalid_fields.push(name.to_owned());
            }
        }
        if mode == EnhancedValidationMode::Strict && !value.invalid_fields.is_empty() {
            return Err(format!(
                "invalid Enhanced RTMP connect field shapes: {}",
                value.invalid_fields.join(", ")
            ));
        }
        Ok(value)
    }

    /// Capability fields safe to forward on a new RTMP connection.
    pub fn forwardable_properties(&self) -> HashMap<String, Amf0Value> {
        let mut properties = HashMap::new();
        for (name, value) in [
            ("capsEx", self.caps_ex.as_ref()),
            ("fourCcList", self.four_cc_list.as_ref()),
            ("videoFourCcInfoMap", self.video_four_cc_info_map.as_ref()),
            ("audioFourCcInfoMap", self.audio_four_cc_info_map.as_ref()),
            ("videoFunction", self.video_function.as_ref()),
            ("audioFunction", self.audio_function.as_ref()),
        ] {
            if let Some(value) = value {
                properties.insert(name.to_owned(), value.clone());
            }
        }
        properties
    }
}

fn valid_four_cc_list(value: &Amf0Value) -> bool {
    let Amf0Value::StrictArray(values) = value else {
        return false;
    };
    values.iter().all(|value| match value {
        Amf0Value::Utf8String(value) => value == "*" || value.len() == 4,
        _ => false,
    })
}

fn valid_four_cc_info_map(value: &Amf0Value) -> bool {
    let Amf0Value::Object(values) = value else {
        return false;
    };
    values
        .iter()
        .all(|(four_cc, flags)| (four_cc == "*" || four_cc.len() == 4) && valid_mask(flags, 0x07))
}

fn valid_mask(value: &Amf0Value, allowed_bits: u32) -> bool {
    valid_u32(value)
        && matches!(value, Amf0Value::Number(value) if (*value as u32) & !allowed_bits == 0)
}

fn valid_u32(value: &Amf0Value) -> bool {
    matches!(
        value,
        Amf0Value::Number(value)
            if value.is_finite()
                && value.fract() == 0.0
                && (0.0..=f64::from(u32::MAX)).contains(value)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_forward_connection_specific_properties() {
        let properties = HashMap::from([
            ("capsEx".into(), Amf0Value::Number(1.0)),
            (
                "tcUrl".into(),
                Amf0Value::Utf8String("rtmp://edge/live".into()),
            ),
        ]);
        let capabilities = EnhancedCapabilities::from_connect_properties(&properties);
        assert_eq!(capabilities.forwardable_properties().len(), 1);
        assert!(capabilities.unknown_properties.contains_key("tcUrl"));
    }

    #[test]
    fn validates_known_connect_field_shapes() {
        let properties = HashMap::from([("capsEx".into(), Amf0Value::Boolean(true))]);
        assert!(EnhancedCapabilities::parse(&properties, EnhancedValidationMode::Strict).is_err());
        let value =
            EnhancedCapabilities::parse(&properties, EnhancedValidationMode::Passthrough).unwrap();
        assert_eq!(value.invalid_fields, ["capsEx"]);
        assert_eq!(
            value.forwardable_properties().get("capsEx"),
            Some(&Amf0Value::Boolean(true))
        );

        for invalid in [Amf0Value::Number(1.5), Amf0Value::Number(16.0)] {
            let properties = HashMap::from([("capsEx".into(), invalid)]);
            assert!(
                EnhancedCapabilities::parse(&properties, EnhancedValidationMode::Strict).is_err()
            );
        }
    }
}
