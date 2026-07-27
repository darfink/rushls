use ffmpeg_sys_next as ffmpeg;
use thiserror::Error;

use crate::domain::Timebase;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RationalError {
    #[error("FFmpeg rational numerator is not positive or representable")]
    Numerator,
    #[error("FFmpeg rational denominator is not positive or representable")]
    Denominator,
}

pub fn to_av_rational(value: Timebase) -> Result<ffmpeg::AVRational, RationalError> {
    Ok(ffmpeg::AVRational {
        num: i32::try_from(value.num().get()).map_err(|_| RationalError::Numerator)?,
        den: i32::try_from(value.den().get()).map_err(|_| RationalError::Denominator)?,
    })
}

pub fn from_av_rational(value: ffmpeg::AVRational) -> Result<Timebase, RationalError> {
    let numerator = u32::try_from(value.num)
        .ok()
        .and_then(std::num::NonZero::new)
        .ok_or(RationalError::Numerator)?;
    let denominator = u32::try_from(value.den)
        .ok()
        .and_then(std::num::NonZero::new)
        .ok_or(RationalError::Denominator)?;
    Ok(Timebase::new(numerator, denominator))
}
