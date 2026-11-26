//! Just enough Prometheus text exposition to read what the suite emits.
//!
//! Written here rather than taken as a dependency because what is needed is small and
//! the alternative is a parser crate in the tree for four functions. What it is for: the
//! suite is **already instrumented** — `worker_stage_duration_seconds`,
//! `dag_node_duration_seconds`, `model_request_duration_seconds` and
//! `model_tokens_total` all exist — so a harness should scrape those rather than time
//! from the outside and call the difference a measurement.
//!
//! Note the exposition carries `# TYPE` but no `# HELP`: nothing in this workspace calls
//! the `describe_*` macros, so a reader that keys on HELP finds nothing.

use std::collections::BTreeMap;

/// One series: a metric name and its labels.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Series {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

/// A parsed exposition.
#[derive(Debug, Clone, Default)]
pub struct Exposition {
    pub samples: Vec<(Series, f64)>,
}

impl Exposition {
    pub fn parse(text: &str) -> Self {
        let mut samples = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((head, value)) = line.rsplit_once(' ') else { continue };
            let Ok(value) = value.parse::<f64>() else { continue };
            let (name, labels) = match head.split_once('{') {
                Some((n, rest)) => (n.to_string(), parse_labels(rest.trim_end_matches('}'))),
                None => (head.to_string(), BTreeMap::new()),
            };
            samples.push((Series { name, labels }, value));
        }
        Self { samples }
    }

    /// Every sample of one metric.
    pub fn metric<'a>(&'a self, name: &'a str) -> impl Iterator<Item = (&'a Series, f64)> {
        self.samples.iter().filter(move |(s, _)| s.name == name).map(|(s, v)| (s, *v))
    }

    /// The sum of one metric over every label set, which is what most of the suite's
    /// counters are wanted as.
    pub fn sum(&self, name: &str) -> f64 {
        self.metric(name).map(|(_, v)| v).sum()
    }

    /// One sample, selected by an exact label match on the labels given.
    pub fn get(&self, name: &str, want: &[(&str, &str)]) -> Option<f64> {
        self.metric(name)
            .find(|(s, _)| want.iter().all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v)))
            .map(|(_, v)| v)
    }

    pub fn has(&self, name: &str) -> bool {
        self.metric(name).next().is_some()
    }

    /// Linear-interpolated quantile of a histogram, from its `_bucket` series.
    ///
    /// Interpolated within the bucket because the shared bucket bounds are coarse
    /// (`0.005 … 600 s`): reporting the bucket's upper bound would turn every page under
    /// 5 ms into "5 ms" and make a regression invisible until it crossed a boundary.
    /// Within the lowest bucket the lower edge is taken as 0.
    pub fn histogram_quantile(&self, metric: &str, labels: &[(&str, &str)], q: f64) -> Option<f64> {
        let bucket = format!("{metric}_bucket");
        let mut points: Vec<(f64, f64)> = self
            .metric(&bucket)
            .filter(|(s, _)| labels.iter().all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v)))
            .filter_map(|(s, v)| {
                let le = s.labels.get("le")?;
                let le = if le == "+Inf" { f64::INFINITY } else { le.parse().ok()? };
                Some((le, v))
            })
            .collect();
        points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let total = points.last()?.1;
        if total <= 0.0 {
            return None;
        }
        let target = q * total;
        let mut prev = (0.0f64, 0.0f64);
        for (le, count) in points {
            if count >= target {
                if le.is_infinite() {
                    return Some(prev.0);
                }
                let span = count - prev.1;
                let frac = if span > 0.0 { (target - prev.1) / span } else { 0.0 };
                return Some(prev.0 + (le - prev.0) * frac);
            }
            prev = (le, count);
        }
        None
    }

    /// Difference of this exposition against an earlier one, per series.
    ///
    /// Counters are cumulative, so a per-phase number is a delta. This is also how tokens
    /// per document are obtained, which `RunRecord::tokens` cannot supply — no token count
    /// reaches the executor, but `model_tokens_total` is incremented at the model client.
    pub fn delta(&self, before: &Exposition) -> Exposition {
        let mut base: BTreeMap<&Series, f64> = BTreeMap::new();
        for (s, v) in &before.samples {
            base.insert(s, *v);
        }
        Exposition {
            samples: self
                .samples
                .iter()
                .map(|(s, v)| (s.clone(), v - base.get(s).copied().unwrap_or(0.0)))
                .collect(),
        }
    }
}

fn parse_labels(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some((k, after)) = rest.split_once('=') else { break };
        let after = after.trim_start_matches('"');
        let Some((v, tail)) = after.split_once('"') else { break };
        out.insert(k.trim().trim_start_matches(',').to_string(), v.to_string());
        rest = tail.trim_start_matches(',');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# TYPE worker_stage_total counter
worker_stage_total{service="ds-suite",stage="page",outcome="ok"} 2
worker_stage_total{service="ds-suite",stage="doc",outcome="ok"} 1
# TYPE model_tokens_total counter
model_tokens_total{service="ds-suite",model="extract-llm",kind="prompt"} 128
model_tokens_total{service="ds-suite",model="extract-llm",kind="completion"} 14
# TYPE worker_stage_duration_seconds histogram
worker_stage_duration_seconds_bucket{stage="page",le="0.005"} 0
worker_stage_duration_seconds_bucket{stage="page",le="0.01"} 2
worker_stage_duration_seconds_bucket{stage="page",le="0.025"} 4
worker_stage_duration_seconds_bucket{stage="page",le="+Inf"} 4
worker_stage_duration_seconds_sum{stage="page"} 0.05
worker_stage_duration_seconds_count{stage="page"} 4
"#;

    #[test]
    fn labels_and_sums_are_read() {
        let e = Exposition::parse(SAMPLE);
        assert_eq!(e.get("worker_stage_total", &[("stage", "page")]), Some(2.0));
        assert_eq!(e.sum("model_tokens_total"), 142.0);
        assert!(e.has("worker_stage_duration_seconds_count"));
        assert!(!e.has("nothing_like_this"));
    }

    #[test]
    fn a_quantile_is_interpolated_inside_its_bucket() {
        let e = Exposition::parse(SAMPLE);
        // 4 observations; the median falls in the 0.005..0.01 bucket, which holds the
        // first two. Taking the bucket's upper bound would say 10 ms for everything in
        // it, which is how a real regression stays invisible.
        let p50 = e.histogram_quantile("worker_stage_duration_seconds", &[("stage", "page")], 0.5).expect("p50");
        assert!(p50 > 0.005 && p50 <= 0.01, "p50 should be inside the second bucket, got {p50}");
        let p95 = e.histogram_quantile("worker_stage_duration_seconds", &[("stage", "page")], 0.95).expect("p95");
        assert!(p95 > p50);
    }

    #[test]
    fn a_delta_is_what_one_phase_did() {
        let before = Exposition::parse(SAMPLE);
        let after = Exposition::parse(&SAMPLE.replace(r#"kind="prompt"} 128"#, r#"kind="prompt"} 200"#));
        let d = after.delta(&before);
        assert_eq!(d.get("model_tokens_total", &[("kind", "prompt")]), Some(72.0));
        assert_eq!(d.get("model_tokens_total", &[("kind", "completion")]), Some(0.0));
    }
}
