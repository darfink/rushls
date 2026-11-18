//! Groups samples by family and writes classic Prometheus text exposition.
use crate::observe::{DURATION_BUCKETS, DurationHistogram, MetricKind, Series};
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
        self.scalar(name, help, MetricKind::Gauge, labels, value);
    }

    /// A missing observation has no sample. A measured zero still does.
    pub fn optional_gauge(
        &mut self,
        name: &'static str,
        help: &'static str,
        labels: &str,
        value: Option<f64>,
    ) {
        if let Some(value) = value {
            self.gauge(name, help, labels, value);
        }
    }

    pub fn counter(
        &mut self,
        name: &'static str,
        help: &'static str,
        labels: &str,
        value: impl Display,
    ) {
        self.scalar(name, help, MetricKind::Counter, labels, value);
    }
    /// Describe an optional family even when no label sets currently exist.
    pub fn describe(&mut self, name: &'static str, help: &'static str, kind: MetricKind) {
        self.0
            .entry(name)
            .or_insert_with(|| (help, kind.as_str(), String::new()));
    }

    /// Render a declarative counter bank through the same path as other samples.
    pub fn series<S>(&mut self, series: &[Series<S>], labels: &str, source: &S) {
        for series in series {
            self.scalar(
                series.name,
                series.help,
                series.kind,
                labels,
                (series.read)(source),
            );
        }
    }

    fn scalar(
        &mut self,
        name: &'static str,
        help: &'static str,
        kind: MetricKind,
        labels: &str,
        value: impl Display,
    ) {
        let (_, _, output) = self
            .0
            .entry(name)
            .or_insert_with(|| (help, kind.as_str(), String::new()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::Reading;
    use std::time::Duration;

    #[test]
    fn declared_and_dynamic_samples_share_one_typed_family() {
        let series = [Series {
            name: "test_total",
            help: "An integral counter.",
            kind: MetricKind::Counter,
            read: |value: &u64| Reading::Integer(*value),
        }];
        let mut samples = Samples::default();
        samples.describe("test_empty", "No active labels.", MetricKind::Gauge);
        samples.describe(series[0].name, series[0].help, series[0].kind);
        samples.series(&series, "stream=\"first\"", &u64::MAX);
        samples.counter("test_total", "An integral counter.", "stream=\"second\"", 2);
        samples.gauge("test_gauge", "No labels.", "", 0.25);
        samples.optional_gauge("test_lag", "Observed lag.", "stream=\"first\"", Some(0.0));
        samples.optional_gauge("test_lag", "Observed lag.", "stream=\"second\"", None);
        let output = samples.finish();
        assert_eq!(output.matches("# HELP test_total ").count(), 1);
        assert_eq!(output.matches("# TYPE test_total counter\n").count(), 1);
        assert!(output.contains("test_total{stream=\"first\"} 18446744073709551615\n"));
        assert!(output.contains("test_total{stream=\"second\"} 2\n"));
        assert!(output.contains("test_gauge 0.25\n"));
        assert!(output.contains("test_lag{stream=\"first\"} 0\n"));
        assert!(!output.contains("test_lag{stream=\"second\"}"));
        assert!(output.contains("# TYPE test_empty gauge\n"));
        assert!(!output.contains("test_empty{"));
    }

    #[test]
    fn histogram_buckets_remain_cumulative_with_overflow_and_labels() {
        let mut histogram = DurationHistogram::default();
        for value in [
            Duration::ZERO,
            Duration::from_millis(5),
            Duration::from_secs(121),
        ] {
            histogram.observe(value);
        }
        let mut samples = Samples::default();
        samples.histogram("test_seconds", "Duration.", "codec=\"h264\"", &histogram);
        let output = samples.finish();
        assert!(output.contains("# TYPE test_seconds histogram\n"));
        assert!(output.contains("test_seconds_bucket{codec=\"h264\",le=\"0.005\"} 2\n"));
        assert!(output.contains("test_seconds_bucket{codec=\"h264\",le=\"120\"} 2\n"));
        assert!(output.contains("test_seconds_bucket{codec=\"h264\",le=\"+Inf\"} 3\n"));
        assert!(output.contains("test_seconds_count{codec=\"h264\"} 3\n"));
        assert!(output.contains("test_seconds_sum{codec=\"h264\"} 121.005\n"));
    }
}
