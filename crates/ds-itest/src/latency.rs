//! Latency, judged against this host rather than against a number.
//!
//! Nothing in this system is sized — the README says so — so there is no absolute target
//! to assert against, and an absolute budget would be a number invented here and then
//! defended. What *is* assertable is that this host has not become slower at the same
//! work than it was, so every assertion is a **ratio against a baseline recorded on this
//! host**, and a host with no baseline reports and skips rather than fails.
//!
//! ## What is measured, and why these three
//!
//! The probes are workload-identical rather than synthetic, and they separate the terms
//! that move for different reasons:
//!
//! - `cpu_ms` — a fixed arithmetic loop in this process. The host's single-core speed,
//!   with nothing else in it. It is the divisor that lets a uniformly slower machine pass.
//! - `api_ms` — `GET /readyz`, which inside the service is a Postgres round trip and a
//!   blob-store round trip. The deployment's own request path, without the model tier.
//! - `model_ms` — one fixed chat completion against the model endpoint. On any real tier
//!   this dominates by orders of magnitude, which is exactly why it is measured
//!   separately: folded into one "host speed" scalar, swapping a GPU would read as a code
//!   regression.
//!
//! ## What is *not* measured here, and cannot be yet
//!
//! The quantity actually worth gating is the **model-free critical path** through a page's
//! DAG: it tests this suite's own code and nothing else. It is not reconstructible from
//! the API. `PageResult` carries per-block provenance — so the duration of each node that
//! produced a block is there — but it carries no node traces, so a node that produced no
//! block (a conditioner, a layout step) leaves no record, and the executor's level
//! structure is not recoverable. Carrying traces in `PageResult` is a `schema_version`
//! bump and a decision, not a task; until then `document_ms` and `model_seconds` are
//! reported and the split is not asserted. `SPEC.md` keeps that open.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::promtext::Exposition;
use crate::Stack;

/// What this host is, and how fast it did three fixed things.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub cpu_model: String,
    pub cores: usize,
    pub tier: String,
    /// Median of a fixed arithmetic loop, milliseconds.
    pub cpu_ms: f64,
    /// Median `GET /readyz`, milliseconds — the service's own request path.
    pub api_ms: f64,
}

impl Probe {
    /// A key for the baseline file: the host and the tier together, because a number from
    /// the `fake` tier and a number from `gpu` are not comparable and must not land in
    /// the same file.
    pub fn host_key(&self) -> String {
        // djb2 over the identifying fields. A hash only has to be stable and spread; a
        // cryptographic one would be a dependency for nothing.
        let mut h: u64 = 5381;
        for b in format!("{}|{}|{}", self.cpu_model, self.cores, self.tier).bytes() {
            h = h.wrapping_mul(33) ^ b as u64;
        }
        format!("{h:016x}")
    }
}

/// Repeats, discarding the first — the run that pays for cold caches, a cold connection
/// and, on a real tier, the model's first-request graph capture.
const WARMUP: usize = 1;
const REPEATS: usize = 5;

/// Documents discarded before timing begins.
///
/// Two, not one. One was the first guess and the measurements disagreed with it: a
/// baseline recorded right after `stack.sh up` came out at 564 ms p50 while every later
/// run on the same host sat at 293–421 ms, so one document was not enough to absorb a
/// freshly started stack and the baseline encoded the cold state instead of the host.
pub const WARMUP_DOCUMENTS: usize = 2;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

/// A fixed arithmetic loop. Deterministic, allocation-free, and `black_box`ed at the end
/// so the optimiser cannot delete the thing being timed.
fn cpu_work() -> f64 {
    let t = Instant::now();
    let mut acc: u64 = 0;
    for i in 0..3_000_000u64 {
        acc = acc.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i | 1);
        acc ^= acc >> 29;
    }
    std::hint::black_box(acc);
    t.elapsed().as_secs_f64() * 1000.0
}

/// Runs the probes.
pub async fn probe(stack: &Stack) -> anyhow::Result<Probe> {
    // The **minimum**, over more repeats than anything else here gets. A CPU benchmark's
    // error is one-sided — contention can only ever make it slower — so the fastest sample
    // is the best estimate of what the host can do, while a median carries whatever else
    // happened to be running. This is a correction: with the median, a run came out 30%
    // faster than its baseline and was reported as a 69% regression, because the
    // baseline's probe had been taken while the stack was starting.
    let cpu_ms = (0..(WARMUP + REPEATS * 4))
        .map(|_| cpu_work())
        .skip(WARMUP)
        .fold(f64::INFINITY, f64::min);

    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let mut api = Vec::new();
    for _ in 0..(WARMUP + REPEATS) {
        let t = Instant::now();
        let res = http.get(format!("{}/readyz", stack.base_url)).send().await?;
        anyhow::ensure!(res.status().is_success(), "/readyz answered {}", res.status());
        api.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    api.drain(..WARMUP);

    Ok(Probe { cpu_model: cpu_model(), cores: cores(), tier: stack.tier.clone(), cpu_ms, api_ms: median(api) })
}

/// The model tier's own p50, read from its histogram after work has gone through.
///
/// Read here and not in `probe`, because the probe runs before the first document and on
/// a freshly started stack the histogram is empty — so a probe-time read reported `None`
/// almost always, which is a measurement that looks like a missing feature. The harness
/// is on the host and the pools are inside the compose network, so the service's own
/// `model_request_duration_seconds` is the right source rather than a direct call.
pub fn model_p50_ms(after: &Exposition) -> Option<f64> {
    after.histogram_quantile("model_request_duration_seconds", &[], 0.5).map(|s| s * 1000.0)
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
}

/// A recorded run: the probe, plus what the stage histograms said.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub probe: Probe,
    /// p50 of `worker_stage_duration_seconds` per stage, in milliseconds.
    pub stage_p50_ms: BTreeMap<String, f64>,
    /// p50 and p95 of per-document wall time from the job state ladder.
    pub document_p50_ms: f64,
    pub document_p95_ms: f64,
    /// The model tier's own p50 for this run, milliseconds.
    #[serde(default)]
    pub model_p50_ms: Option<f64>,
    /// Share of a document's time that was model time. The assertion that earns its keep:
    /// it catches non-model work added to the hot path, which no timing can distinguish
    /// from "the model tier is slower today".
    pub model_share: f64,
    /// How many documents the figures came from. Printed everywhere they are, because a
    /// p95 from four documents is the maximum with a large standard error.
    pub documents: usize,
}

impl Baseline {
    pub fn path(root: &std::path::Path, key: &str) -> std::path::PathBuf {
        root.join("deploy/compose/baselines").join(format!("{key}.json"))
    }

    pub fn load(root: &std::path::Path, key: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(root, key)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, root: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        let p = Self::path(root, &self.probe.host_key());
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(p)
    }
}

/// Why a measurement does not count.
///
/// A latency number taken while a page was re-rendered, a request retried or an endpoint
/// ejected is not a measurement of this code. Such a run is reported **invalid** rather
/// than failed: the host was busy or the tier was unwell, and failing would train everyone
/// to ignore the test.
pub fn invalidations(delta: &Exposition) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |metric: &str, what: &str| {
        let n = delta.sum(metric);
        if n > 0.0 {
            out.push(format!("{what} ({metric} = {n})"));
        }
    };
    check("dag_node_failures_total", "a DAG node failed");
    check("worker_page_rerenders_total", "a page was rendered twice");
    check("model_endpoint_ejections_total", "a model endpoint was ejected");
    check("worker_lease_contended_total", "another copy held the lease");
    check("suite_jobs_resumed_total", "a job was resumed from a lapsed lease");
    out
}

/// The outcome of comparing a run with a baseline.
#[derive(Debug, Clone)]
pub struct Comparison {
    pub name: String,
    pub baseline: f64,
    pub measured: f64,
    /// Measured over baseline, after normalising by how much slower this host's CPU is.
    pub normalised_ratio: f64,
    pub tolerance: f64,
    /// Whether this comparison is allowed to fail the run at all. A quantity below
    /// `MIN_COMPARABLE_MS` is reported and not gated.
    pub gated: bool,
}

impl Comparison {
    /// Within tolerance, or not gated in the first place.
    pub fn within(&self) -> bool {
        !self.gated || self.normalised_ratio <= self.tolerance
    }

    /// Faster than the baseline by more than noise explains. Not a pass to be pleased
    /// about: either work was skipped or the baseline is stale.
    pub fn suspiciously_fast(&self) -> bool {
        self.normalised_ratio < SUSPICIOUSLY_FAST
    }
}

/// Below this many documents a tail percentile is the maximum with a huge standard
/// error, so it is reported and not compared.
///
/// Measured, not guessed: across seven documents on one quiet host the maximum ranged
/// from 418 ms to 6221 ms between runs while the median stayed inside 293–421 ms. A gate
/// on that tail would fail on a hiccup and teach everyone to ignore the test.
pub const MIN_DOCUMENTS_FOR_TAIL: usize = 60;

/// A run this much faster than its baseline is not celebrated, it is suspected.
///
/// Work that was skipped is the easiest false improvement there is, and a stale baseline
/// — one recorded on a cold stack — is the next easiest. Either way the number is not
/// evidence of an improvement, and the caller is told so.
pub const SUSPICIOUSLY_FAST: f64 = 0.7;

/// How far the host-speed normalisation may move a ratio.
///
/// A backstop, deliberately wide. Two different things guard the divisor, and they are
/// not interchangeable:
///
/// - The probe itself is the **minimum** of twenty runs, which is what actually fixes
///   noise: contention can only make a CPU benchmark slower, so the fastest sample is the
///   honest one. That is what stopped a 30%-faster run from reading as a 69% regression.
/// - This clamp only catches a reading so absurd that no real host difference explains
///   it, and reports it rather than acting on it.
///
/// It is wide because a genuinely slower machine must still normalise *fully*: a laptop on
/// battery, or a shared CI runner against a workstation, is easily two or three times
/// slower at the same work, and refusing to normalise that away would fail the test for
/// the one reason this whole design exists to tolerate. A narrow clamp was tried first
/// and did exactly that.
pub const MAX_CPU_FACTOR: f64 = 3.0;

/// Below this, a difference in the CPU probe is treated as noise and not normalised away.
///
/// A deadband, and measured rather than chosen: with the probe stable, two runs minutes
/// apart on an otherwise idle host still differed by up to ~15%, and applying that as a
/// divisor turned an identical 147 ms median into a reported x0.47. A small probe
/// difference is far more likely to be the machine breathing than a real change in how
/// fast it is.
pub const CPU_FACTOR_DEADBAND: f64 = 1.15;

/// Quantities smaller than this are reported but never gated, in milliseconds.
///
/// Measurement granularity dominates a small number: `GET /readyz` runs in about a
/// millisecond here, so half a millisecond of scheduling noise is a 40% "regression" and
/// a gate on it fails for no reason — which it did, on the run that prompted this.
pub const MIN_COMPARABLE_MS: f64 = 10.0;

/// Compares a run with a baseline, normalising host-attributable terms by the CPU probe.
///
/// A host that is uniformly twice as slow passes; a host where one stage got twice as slow
/// does not. That is the whole point of normalising rather than comparing raw times.
pub fn compare(base: &Baseline, now: &Baseline, tolerance: f64) -> Vec<Comparison> {
    compare_with(base, now, tolerance).0
}

/// `compare`, and the raw host-speed factor if it had to be clamped.
pub fn compare_with(base: &Baseline, now: &Baseline, tolerance: f64) -> (Vec<Comparison>, Option<f64>) {
    let raw_factor = if now.probe.cpu_ms > 0.0 && base.probe.cpu_ms > 0.0 { base.probe.cpu_ms / now.probe.cpu_ms } else { 1.0 };
    let clamped_factor = raw_factor.clamp(1.0 / MAX_CPU_FACTOR, MAX_CPU_FACTOR);
    let clamped = (raw_factor != clamped_factor).then_some(raw_factor);
    // Inside the deadband the hosts are the same speed as far as this can tell.
    let cpu_factor = if (1.0 / CPU_FACTOR_DEADBAND..=CPU_FACTOR_DEADBAND).contains(&clamped_factor) { 1.0 } else { clamped_factor };
    let mut out = Vec::new();
    let mut push = |name: &str, b: f64, m: f64, normalise: bool, tol: f64| {
        if b <= 0.0 {
            return;
        }
        let raw = m / b;
        out.push(Comparison {
            name: name.to_string(),
            baseline: b,
            measured: m,
            normalised_ratio: if normalise { raw * cpu_factor } else { raw },
            tolerance: tol,
            gated: b >= MIN_COMPARABLE_MS && m >= MIN_COMPARABLE_MS,
        });
    };
    push("api_ms p50", base.probe.api_ms, now.probe.api_ms, true, tolerance);
    push("document p50", base.document_p50_ms, now.document_p50_ms, true, tolerance);
    // The tail only when there is enough of it to mean anything. Comparing it at n = 7
    // was a mistake this suite's own measurements caught: see MIN_DOCUMENTS_FOR_TAIL.
    if base.documents >= MIN_DOCUMENTS_FOR_TAIL && now.documents >= MIN_DOCUMENTS_FOR_TAIL {
        push("document p95", base.document_p95_ms, now.document_p95_ms, true, tolerance * 1.5);
    }
    for (stage, b) in &base.stage_p50_ms {
        if let Some(m) = now.stage_p50_ms.get(stage) {
            push(&format!("stage {stage} p50"), *b, *m, true, tolerance);
        }
    }
    (out, clamped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_with(cpu_ms: f64) -> Probe {
        Probe { cpu_model: "test".into(), cores: 8, tier: "fake".into(), cpu_ms, api_ms: 10.0 }
    }

    fn baseline(cpu_ms: f64, doc: f64) -> Baseline {
        Baseline {
            probe: probe_with(cpu_ms),
            stage_p50_ms: [("page".to_string(), 20.0)].into_iter().collect(),
            document_p50_ms: doc,
            document_p95_ms: doc * 2.0,
            model_p50_ms: Some(5.0),
            model_share: 0.5,
            documents: 4,
        }
    }

    #[test]
    fn a_uniformly_slower_host_passes_and_a_single_regression_does_not() {
        let base = baseline(100.0, 200.0);

        // Everything twice as slow, including the CPU probe: the same machine under load,
        // or a slower one. Normalising by the probe says this is not a regression.
        let mut slower = baseline(200.0, 400.0);
        slower.probe.api_ms = 20.0;
        slower.stage_p50_ms.insert("page".into(), 40.0);
        for c in compare(&base, &slower, 1.3) {
            assert!(c.within(), "{} should pass on a uniformly slower host: ratio {}", c.name, c.normalised_ratio);
        }

        // The CPU is as fast as before, but a document now takes twice as long. That is a
        // regression and nothing normalises it away.
        let mut regressed = baseline(100.0, 400.0);
        regressed.stage_p50_ms.insert("page".into(), 20.0);
        let doc = compare(&base, &regressed, 1.3).into_iter().find(|c| c.name == "document p50").expect("compared");
        assert!(!doc.within(), "a doubled document time must not pass: ratio {}", doc.normalised_ratio);
    }

    /// An absurd host-speed factor is reported rather than acted on.
    ///
    /// The scenario is real and this file produced it: a baseline whose CPU probe was
    /// taken while the stack was starting read far slower than the host, and a run 30%
    /// faster than that baseline was reported as a 69% regression. The probe is now the
    /// minimum of twenty runs, which is what prevents it; this clamp is the backstop for
    /// a reading no host difference explains.
    #[test]
    fn an_absurd_host_factor_is_clamped_and_reported() {
        // A probe five times apart is not a host difference, it is a broken measurement.
        let (comparisons, clamped) = compare_with(&baseline(500.0, 173.0), &baseline(100.0, 121.0), 1.4);
        assert_eq!(clamped, Some(5.0), "the raw factor must be surfaced, not silently applied");
        let doc = comparisons.iter().find(|c| c.name == "document p50").expect("compared");
        assert!(doc.normalised_ratio <= (121.0 / 173.0) * MAX_CPU_FACTOR + 1e-9, "the factor is bounded by the clamp");

        // Within the band nothing is clamped, and a real regression still fails: same
        // host speed, twice as slow.
        let (slow, clamped) = compare_with(&baseline(100.0, 173.0), &baseline(100.0, 400.0), 1.4);
        assert!(clamped.is_none());
        assert!(!slow.iter().find(|c| c.name == "document p50").expect("compared").within());
    }

    /// Two flaps this suite produced, both pinned.
    #[test]
    fn tiny_quantities_and_small_probe_drift_do_not_fail_a_run() {
        // `GET /readyz` runs in about a millisecond, so half a millisecond of scheduling
        // noise is a 45% ratio. Reported, never gated.
        let mut base = baseline(100.0, 173.0);
        let mut now = baseline(100.0, 176.0);
        base.probe.api_ms = 1.1;
        now.probe.api_ms = 1.6;
        let api = compare(&base, &now, 1.4).into_iter().find(|c| c.name == "api_ms p50").expect("compared");
        assert!(!api.gated, "a one-millisecond quantity must not gate");
        assert!(api.within(), "and so must not fail the run");

        // The probe drifting 15% on an idle host turned an identical median into x0.47.
        // Inside the deadband the two hosts count as the same speed.
        let drifted = compare(&baseline(100.0, 173.0), &baseline(114.0, 173.0), 1.4);
        let doc = drifted.iter().find(|c| c.name == "document p50").expect("compared");
        assert!((doc.normalised_ratio - 1.0).abs() < 1e-9, "identical work must read as x1.0, got {}", doc.normalised_ratio);

        // Beyond the deadband a real host difference still normalises.
        let slower = compare(&baseline(100.0, 173.0), &baseline(200.0, 346.0), 1.4);
        let doc = slower.iter().find(|c| c.name == "document p50").expect("compared");
        assert!(doc.within(), "a genuinely 2x slower host still passes: {}", doc.normalised_ratio);
    }

    #[test]
    fn a_host_key_separates_tiers() {
        let mut fake = probe_with(100.0);
        let mut gpu = probe_with(100.0);
        gpu.tier = "gpu".into();
        assert_ne!(fake.host_key(), gpu.host_key(), "a fake number and a gpu number must not share a baseline");
        fake.cores = 4;
        assert_ne!(fake.host_key(), probe_with(100.0).host_key(), "a different core count is a different host");
    }

    #[test]
    fn a_busy_run_is_reported_invalid_rather_than_failed() {
        let clean = Exposition::parse("dag_node_failures_total{a=\"b\"} 0\nworker_page_rerenders_total 0\n");
        assert!(invalidations(&clean).is_empty());
        let busy = Exposition::parse("worker_page_rerenders_total 2\nmodel_endpoint_ejections_total 1\n");
        let why = invalidations(&busy);
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(why.iter().any(|w| w.contains("rendered twice")));
    }
}
