//! Publisher timing declarations, kept separate from nominal rate hints.
use super::FrameRate;
use derive_more::Display;

#[derive(Clone, Copy, Debug, Default, Display, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[display(rename_all = "lowercase")]
pub enum InputMode {
    #[default]
    Permissive,
    Strict,
}
impl std::str::FromStr for InputMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "permissive" => Ok(Self::Permissive),
            "strict" => Ok(Self::Strict),
            _ => Err("input_mode must be permissive or strict".into()),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DecoderConfigOrigin {
    #[default]
    Publisher,
    Synthesized,
}
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum CadenceSource {
    #[display("h264_vui")]
    H264Vui,
    HevcSpsHrd,
    HevcVpsHrd,
    HevcConfiguration,
    #[display("av1_sequence")]
    Av1Sequence,
}
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum CadenceUnavailable {
    PictureStructure,
    TemporalLayers,
    ParameterSets,
    MissingExactInterval,
    DisplayMapping,
    InvalidTiming,
    TimestampPrecision,
}
#[derive(Clone, Copy, Debug, Display, Eq, PartialEq)]
#[display(rename_all = "snake_case")]
pub enum CadenceScope {
    ProgressiveFrames,
    ProgressiveBaseLayer,
    SingleLayerTemporalUnits,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoCadence {
    #[default]
    Unknown,
    Nominal(FrameRate),
    Fixed {
        rate: FrameRate,
        source: CadenceSource,
        scope: CadenceScope,
    },
    Unverifiable {
        rate: Option<FrameRate>,
        source: CadenceSource,
        reason: CadenceUnavailable,
    },
    Conflicting {
        source: CadenceSource,
    },
}
impl VideoCadence {
    pub fn rate(self) -> Option<FrameRate> {
        match self {
            Self::Fixed { rate, .. } | Self::Nominal(rate) => Some(rate),
            Self::Unverifiable { rate, .. } => rate,
            _ => None,
        }
    }
    pub fn source(self) -> Option<CadenceSource> {
        match self {
            Self::Fixed { source, .. }
            | Self::Unverifiable { source, .. }
            | Self::Conflicting { source } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoTimestampObservation {
    pub codec: super::Codec,
    pub ticks: u64,
    pub timebase: super::Timebase,
}
