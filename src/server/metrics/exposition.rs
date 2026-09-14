//! Groups samples by family and writes classic Prometheus text exposition.
use crate::observe::{DURATION_BUCKETS, DurationHistogram};
use std::{
    collections::BTreeMap,
    fmt::{Display, Write},
};

#[derive(Default)]
pub struct Samples(BTreeMap<&'static str, (&'static str, &'static str, String)>);
impl Samples {
    pub fn gauge(
        &mut self,
        name: &'static str,
        help: &'static str,
        labels: &str,
        value: impl Display,
    ) {
        self.scalar(name, help, "gauge", labels, value);
    }
    pub fn counter(
        &mut self,
        name: &'static str,
        help: &'static str,
        labels: &str,
        value: impl Display,
    ) {
        self.scalar(name, help, "counter", labels, value);
    }
    fn scalar(
        &mut self,
        name: &'static str,
        help: &'static str,
        kind: &'static str,
        labels: &str,
        value: impl Display,
    ) {
        let (_, _, output) = self
            .0
            .entry(name)
            .or_insert_with(|| (help, kind, String::new()));
        if labels.is_empty() {
            writeln!(output, "{name} {value}")
        } else {
            writeln!(output, "{name}{{{labels}}} {value}")
        }
        .expect("String write");
    }
    pub fn histogram(
        &mut self,
        name: &'static str,
        help: &'static str,
        labels: &str,
        histogram: &DurationHistogram,
    ) {
        let (_, _, output) = self
            .0
            .entry(name)
            .or_insert_with(|| (help, "histogram", String::new()));
        let prefix = if labels.is_empty() {
            String::new()
        } else {
            format!("{labels},")
        };
        let mut cumulative = 0;
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            cumulative += histogram.buckets[index];
            writeln!(
                output,
                "{name}_bucket{{{prefix}le=\"{bound}\"}} {cumulative}"
            )
            .expect("String write");
        }
        writeln!(
            output,
            "{name}_bucket{{{prefix}le=\"+Inf\"}} {}",
            histogram.count
        )
        .expect("String write");
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        writeln!(
            output,
            "{name}_count{labels} {}\n{name}_sum{labels} {}",
            histogram.count, histogram.sum
        )
        .expect("String write");
    }
    pub fn finish(self) -> String {
        let mut output = String::new();
        for (name, (help, kind, samples)) in self.0 {
            writeln!(
                output,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{samples}"
            )
            .expect("String write");
        }
        output
    }
}
