//! Declaring a counter once instead of four times.
//!
//! A counter used to be spelled in four places: the atomic that holds it, the
//! snapshot field that carries it out, the line of `snapshot()` that copies
//! between them, and the exporter's name/help/read triple. Three of those are
//! mechanical, and the fourth lives in a different module — so the failure mode
//! was a counter that increments perfectly and is never exported, which nothing
//! catches because there is no test that can know a series was meant to exist.
//!
//! [`counters!`] declares all four from one line each.
//!
//! # What stays out of here
//!
//! This module describes *what is measured and what it is called*. It does not
//! know what an exposition format is: [`Series`] is a name, a help string, a
//! kind, and a function, and `server::metrics` decides how to serialize them.
//! That keeps the layering the crate already has — `observe` sits at the bottom
//! and names no transport — while still keeping a counter adjacent to the name
//! it is exported under, which is the part that was worth fixing.

use std::{
    fmt,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

/// Whether a series only ever climbs, or moves in both directions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
}

impl MetricKind {
    /// The name exposition formats use for this kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// One reading, in the shape it was measured in.
///
/// Kept apart from `f64` so an integral counter prints as an integer. Every
/// exposition format accepts a decimal point, but an operator reading
/// `rushls_source_payload_bytes_total 1024` should not be shown `1024.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    Integer(u64),
    Count(usize),
    Seconds(f64),
}

impl fmt::Display for Reading {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(value) => write!(output, "{value}"),
            Self::Count(value) => write!(output, "{value}"),
            Self::Seconds(value) => write!(output, "{value}"),
        }
    }
}

impl From<u64> for Reading {
    fn from(value: u64) -> Self {
        Self::Integer(value)
    }
}

impl From<usize> for Reading {
    fn from(value: usize) -> Self {
        Self::Count(value)
    }
}

impl From<f64> for Reading {
    fn from(value: f64) -> Self {
        Self::Seconds(value)
    }
}

impl From<bool> for Reading {
    fn from(value: bool) -> Self {
        Self::Integer(u64::from(value))
    }
}

/// One exported series: what it is called, what it means, and how to read it
/// out of a snapshot.
pub struct Series<S: 'static> {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: MetricKind,
    pub read: fn(&S) -> Reading,
}

/// A value that has an atomic counterpart to accumulate in.
pub trait Measured: Copy + Into<Reading> {
    type Atomic: Default + fmt::Debug;

    fn load(atomic: &Self::Atomic) -> Self;
}

impl Measured for u64 {
    type Atomic = AtomicU64;

    fn load(atomic: &Self::Atomic) -> Self {
        atomic.load(Ordering::Relaxed)
    }
}

impl Measured for usize {
    type Atomic = AtomicUsize;

    fn load(atomic: &Self::Atomic) -> Self {
        atomic.load(Ordering::Relaxed)
    }
}

/// Declares an atomic counter set, the snapshot it reads out into, and the
/// series both are exported as.
///
/// Increment methods stay hand-written: there are far fewer of them than there
/// are counters, their names deliberately differ from their fields, and several
/// carry the reasoning for why a particular event is counted apart from a
/// neighbouring one — which is exactly the sort of comment a macro would eat.
macro_rules! counters {
    (
        $(#[$snapshot_meta:meta])*
        $counters:ident => $snapshot:ident {
            $(
                $(#[$field_meta:meta])*
                $field:ident: $ty:ty = $kind:ident($name:literal, $help:literal)
            ),+ $(,)?
        }
    ) => {
        #[derive(Debug, Default)]
        struct $counters {
            $($field: <$ty as $crate::observe::counters::Measured>::Atomic,)+
        }

        $(#[$snapshot_meta])*
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        pub struct $snapshot {
            $(
                $(#[$field_meta])*
                pub $field: $ty,
            )+
        }

        impl $counters {
            fn snapshot(&self) -> $snapshot {
                $snapshot {
                    $(
                        $field: <$ty as $crate::observe::counters::Measured>::load(&self.$field),
                    )+
                }
            }
        }

        $crate::observe::counters::series! {
            $snapshot {
                $($kind($name, $help) = |snapshot: &$snapshot| snapshot.$field),+
            }
        }
    };
}

/// Describes the series an already-declared snapshot exports.
///
/// The half of [`counters!`] for a snapshot whose storage is not a plain bank
/// of atomics — durations held as nanoseconds, capacities that come from
/// configuration rather than from measurement.
///
/// A snapshot owned by another crate takes the second form, which names the
/// const instead of hanging one off the type: the orphan rule forbids an
/// inherent `impl` on a foreign type, and a shared crate has no business
/// knowing what this application calls its metrics.
macro_rules! series {
    (
        $snapshot:ty {
            $(
                $kind:ident($name:literal, $help:literal) = $read:expr
            ),+ $(,)?
        }
    ) => {
        impl $snapshot {
            /// Every series this snapshot exports, in declaration order.
            pub const SERIES: &'static [$crate::observe::counters::Series<Self>] = &[
                $($crate::observe::counters::Series {
                    name: $name,
                    help: $help,
                    kind: $crate::observe::counters::MetricKind::$kind,
                    read: |snapshot: &$snapshot| {
                        $crate::observe::counters::Reading::from($read(snapshot))
                    },
                },)+
            ];
        }
    };

    (
        $(#[$meta:meta])*
        $visibility:vis $series:ident: $snapshot:ty {
            $(
                $kind:ident($name:literal, $help:literal) = $read:expr
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $visibility const $series: &[$crate::observe::counters::Series<$snapshot>] = &[
            $($crate::observe::counters::Series {
                name: $name,
                help: $help,
                kind: $crate::observe::counters::MetricKind::$kind,
                read: |snapshot: &$snapshot| {
                    $crate::observe::counters::Reading::from($read(snapshot))
                },
            },)+
        ];
    };
}

pub(crate) use counters;
pub(crate) use series;
