//! Predicates over the media a publisher may offer.
//!
//! Every field an operator writes under `[publish]` is a set of admitted values
//! rather than a single ceiling. Three constructors cover the whole surface —
//! exact, one-of, and an inclusive range — so a rule reads the same way
//! wherever it appears.
//!
//! A bare scalar is **exact, not a ceiling**. `tracks = 1` admits exactly one
//! track and "at most one" is `{ max = 1 }`. The alternative was argued and
//! rejected on one case: `channels = 2` reads as a specification while
//! `tracks = 8` reads as a budget, and a rule correct on four fields and
//! misleading on the fifth is worse than an explicit form everywhere.

use std::fmt;

use crate::domain::Codec;

/// The values one media property may take.
///
/// Generic over the property so ordering, equality, and the error message all
/// come from the domain type rather than from a parallel hierarchy per field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Bounds<T> {
    /// Every value. What omitting the field means.
    #[default]
    Any,
    /// Exactly this value.
    Exact(T),
    /// Any one of these. Arrays are set membership, never a range:
    /// enumerations are the common case in this domain, and spending array
    /// syntax on ranges would leave no honest way to spell them.
    OneOf(Vec<T>),
    /// An inclusive range, either end optional.
    Range { min: Option<T>, max: Option<T> },
}

impl<T: PartialOrd + PartialEq> Bounds<T> {
    /// Whether `candidate` is in the admitted set.
    pub fn admits(&self, candidate: &T) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(value) => candidate == value,
            Self::OneOf(values) => values.contains(candidate),
            Self::Range { min, max } => {
                min.as_ref().is_none_or(|min| candidate >= min)
                    && max.as_ref().is_none_or(|max| candidate <= max)
            }
        }
    }

    /// The most permissive bound, for a field an operator did not narrow.
    pub fn any() -> Self {
        Self::Any
    }

    /// "At most this", the shape most numeric budgets want.
    pub fn at_most(max: T) -> Self {
        Self::Range {
            min: None,
            max: Some(max),
        }
    }
}

impl<T: fmt::Display> fmt::Display for Bounds<T> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any
            | Self::Range {
                min: None,
                max: None,
            } => output.write_str("any"),
            Self::Exact(value) => write!(output, "{value}"),
            Self::OneOf(values) => {
                let names: Vec<String> = values.iter().map(ToString::to_string).collect();
                write!(output, "one of [{}]", names.join(", "))
            }
            Self::Range {
                min: Some(min),
                max: None,
            } => write!(output, "at least {min}"),
            Self::Range {
                min: None,
                max: Some(max),
            } => write!(output, "at most {max}"),
            Self::Range {
                min: Some(min),
                max: Some(max),
            } => write!(output, "{min} to {max}"),
        }
    }
}

/// Which codecs are admitted.
///
/// A set rather than a [`Bounds`], because codecs have no ordering worth
/// having: "between H.264 and Opus" is not a question anyone asks, and giving
/// [`Codec`](crate::domain::Codec) an `Ord` purely to reuse the range
/// machinery would invent a total order that means nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Codecs {
    /// Every codec this origin can mux. What omitting the field means, and
    /// deliberately not the same as naming today's set: a codec added by a
    /// later release is admitted here and refused by an explicit list.
    #[default]
    Any,
    OneOf(Vec<Codec>),
}

impl Codecs {
    pub fn admits(&self, candidate: Codec) -> bool {
        match self {
            Self::Any => true,
            Self::OneOf(codecs) => codecs.contains(&candidate),
        }
    }
}

impl fmt::Display for Codecs {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => output.write_str("any codec"),
            Self::OneOf(codecs) => {
                let names: Vec<String> = codecs.iter().map(|codec| format!("{codec:?}")).collect();
                write!(output, "one of [{}]", names.join(", "))
            }
        }
    }
}

/// A frame size, compared as a box rather than on a one-dimensional scale.
///
/// A named size such as `4k` is a bounding box: the frame must fit inside it,
/// with axes swapped for portrait. Anamorphic and portrait sources break any
/// single ranking of "size", so names expand to boxes and then compare
/// numerically — which admits a 2160x3840 portrait source under a 4k bound,
/// where checking width and height independently would refuse it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct FrameBox {
    pub width: u32,
    pub height: u32,
}

impl FrameBox {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Whether this frame fits inside `limit` in either orientation.
    pub fn fits_within(self, limit: Self) -> bool {
        let (long, short) = self.oriented();
        let (limit_long, limit_short) = limit.oriented();
        long <= limit_long && short <= limit_short
    }

    fn oriented(self) -> (u32, u32) {
        if self.width >= self.height {
            (self.width, self.height)
        } else {
            (self.height, self.width)
        }
    }
}

impl fmt::Display for FrameBox {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}x{}", self.width, self.height)
    }
}

/// Which frame sizes are admitted.
///
/// Its own type rather than `Bounds<FrameBox>` because "fits inside" is not
/// the ordering `Ord` gives a pair of numbers, and quietly reusing one for the
/// other is how a portrait source ends up refused by a landscape bound.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Resolution {
    #[default]
    Any,
    /// The frame must fit inside this box, in either orientation.
    AtMost(FrameBox),
    /// The frame must be exactly this size, in either orientation.
    Exact(FrameBox),
}

impl Resolution {
    pub fn admits(&self, candidate: FrameBox) -> bool {
        match self {
            Self::Any => true,
            Self::AtMost(limit) => candidate.fits_within(*limit),
            Self::Exact(size) => {
                candidate == *size || candidate == FrameBox::new(size.height, size.width)
            }
        }
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => output.write_str("any"),
            Self::AtMost(limit) => write!(output, "at most {limit}"),
            Self::Exact(size) => write!(output, "{size}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_value_is_exact_rather_than_a_ceiling() {
        let exact = Bounds::Exact(2_u32);
        assert!(exact.admits(&2));
        assert!(!exact.admits(&1), "an exact bound is not a ceiling");
        assert!(!exact.admits(&3));
    }

    #[test]
    fn a_range_is_inclusive_at_both_ends() {
        let range = Bounds::Range {
            min: Some(24_u32),
            max: Some(60),
        };
        assert!(range.admits(&24));
        assert!(range.admits(&60));
        assert!(!range.admits(&23));
        assert!(!range.admits(&61));
    }

    #[test]
    fn an_open_ended_range_bounds_only_the_end_it_names() {
        let floor = Bounds::Range {
            min: Some(44_100_u32),
            max: None,
        };
        assert!(floor.admits(&48_000));
        assert!(!floor.admits(&32_000));
    }

    #[test]
    fn a_set_is_membership_rather_than_a_span() {
        let rates = Bounds::OneOf(vec![24_u32, 25, 30]);
        assert!(rates.admits(&25));
        assert!(
            !rates.admits(&27),
            "a value between two listed ones is not admitted by either"
        );
    }

    #[test]
    fn a_named_size_admits_the_same_frame_in_either_orientation() {
        let uhd = Resolution::AtMost(FrameBox::new(3840, 2160));
        assert!(uhd.admits(FrameBox::new(3840, 2160)));
        assert!(
            uhd.admits(FrameBox::new(2160, 3840)),
            "a portrait source fits the same box, which checking each axis \
             against its own limit would have refused"
        );
        assert!(uhd.admits(FrameBox::new(1920, 1080)));
        assert!(!uhd.admits(FrameBox::new(4096, 2160)));
    }

    #[test]
    fn an_anamorphic_frame_is_judged_by_its_longest_axis() {
        let hd = Resolution::AtMost(FrameBox::new(1920, 1080));
        assert!(hd.admits(FrameBox::new(1440, 1080)));
        assert!(!hd.admits(FrameBox::new(1920, 1200)));
    }
}
