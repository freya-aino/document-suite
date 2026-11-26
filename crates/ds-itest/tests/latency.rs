//! Latency, as a ratio against what this host did before.
//!
//! Read `src/latency.rs` first; it says why there are no absolute numbers here and what
//! the three probes separate. The short version: nothing in this system is sized, so an
//! absolute budget would be a number invented in this file and then defended, and a raw
//! comparison between two machines says more about the machines than about the code.
//!
//! A host with no baseline **reports and skips**. A new machine must never fail this
//! suite, and the first run on one is how its baseline gets written
//! (`DS_ITEST_WRITE_BASELINE=1`).

use std::time::Duration;

use ds_itest::client::{Client, Upload};
use ds_itest::corpus::{freshened, Corpus};
use ds_itest::latency::{self, Baseline};
use ds_itest::stack::stack_or_skip;

const LIST_VERSION: &str = "example-2026-11-06";
/// Tolerance on a normalised p50 ratio. Wide enough for an ordinarily busy developer
/// machine, narrow enough that a doubling fails.
const TOLERANCE: f64 = 1.4;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the compose stack: deploy/compose/stack.sh up"]
async fn per_document_latency_is_within_this_host_s_baseline() -> anyhow::Result<()> {
    let Some(stack) = stack_or_skip().await else { return Ok(()) };
    let client = Client::login(&stack, "ds-api", "ds-api-dev-secret").await?;
    let list = std::fs::read_to_string(repo_root().join("config/dag-list.example.json"))?;
    client.activate_dag_list(&list, LIST_VERSION).await?;

    let probe = latency::probe(&stack).await?;
    eprintln!("\n── host ──\n{} ({} cores), tier `{}`", probe.cpu_model, probe.cores, probe.tier);
    eprintln!("cpu {:.1} ms · api(/readyz) {:.1} ms", probe.cpu_ms, probe.api_ms);

    let dir = std::env::temp_dir().join(format!("ds-itest-lat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Enough documents that the median has something to stand on after the warm-ups are
    // discarded. The tail still does not — see `latency::MIN_DOCUMENTS_FOR_TAIL`.
    let corpus = Corpus::generate(&dir, 3, 1)?;

    // Fresh bytes per run, or this measures the render cache rather than the renderer:
    // ingest skips a page whose signals already exist, keyed by content hash.
    let nonce = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis());

    let before = client.metrics(&stack).await?;
    let mut document_ms: Vec<f64> = Vec::new();

    for (i, entry) in corpus.entries.iter().enumerate() {
        let opts = Upload { language: entry.language.clone(), profile: "balanced".into(), doc_type: entry.doc_type_hint.clone() };
        let bytes = freshened(&std::fs::read(&entry.uri)?, &format!("{nonce}-{i}"));
        let accepted = client.upload(bytes, &format!("{}.pdf", entry.id()), &opts).await?;
        let job = client.await_job(&accepted.job_id, Duration::from_secs(180)).await?;
        anyhow::ensure!(job.state == "done", "{}: {:?}", entry.id(), job.error);

        // The first documents are discarded: they pay for a cold page cache, a cold
        // connection pool and, on a real tier, the model's first-request graph capture.
        // Keeping them would put a one-off cost into every percentile — and did, in the
        // first baseline this suite recorded.
        if i < latency::WARMUP_DOCUMENTS {
            eprintln!("warm-up document {}: {:.0} ms (discarded)", i + 1, ds_itest::eval::document_ms(&job));
            continue;
        }
        document_ms.push(ds_itest::eval::document_ms(&job) as f64);
    }
    anyhow::ensure!(document_ms.len() >= 2, "too few documents to say anything");

    let after = client.metrics(&stack).await?;
    let delta = after.delta(&before);

    // ── is this a measurement at all? ──
    //
    // Paired with the timing deliberately: a ratio below 1.0 because work was skipped is
    // the easiest false improvement there is.
    let why = latency::invalidations(&delta);
    assert!(delta.sum("dag_pages_total") >= document_ms.len() as f64, "fewer pages ran than documents were uploaded");
    if !why.is_empty() {
        eprintln!("\n!! this run does not count as a measurement: {}", why.join("; "));
        eprintln!("   (reported, not failed: the host was busy or the tier was unwell)");
        return Ok(());
    }

    document_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let stage_p50_ms = ["ingest", "page", "doc"]
        .into_iter()
        .filter_map(|s| {
            after
                .histogram_quantile("worker_stage_duration_seconds", &[("stage", s)], 0.5)
                .map(|v| (s.to_string(), v * 1000.0))
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    let model_seconds = delta.sum("model_request_duration_seconds_sum");
    let doc_seconds: f64 = document_ms.iter().sum::<f64>() / 1000.0;
    let model_share = if doc_seconds > 0.0 { (model_seconds / doc_seconds).clamp(0.0, 1.0) } else { 0.0 };

    let now = Baseline {
        probe: probe.clone(),
        stage_p50_ms,
        document_p50_ms: percentile(&document_ms, 0.5),
        // `document_p95_ms` from a handful of documents **is** the maximum, and is named
        // p95 only because that is the field it fills. It is compared with a looser
        // tolerance for exactly that reason.
        document_p95_ms: percentile(&document_ms, 0.95),
        model_p50_ms: latency::model_p50_ms(&after),
        model_share,
        documents: document_ms.len(),
    };

    eprintln!("\n── this run ({} documents) ──", now.documents);
    eprintln!(
        "document p50 {:.0} ms · max {:.0} ms · model share {:.2} · model p50 {}",
        now.document_p50_ms,
        now.document_p95_ms,
        now.model_share,
        now.model_p50_ms.map(|m| format!("{m:.1} ms")).unwrap_or_else(|| "–".into())
    );
    for (stage, ms) in &now.stage_p50_ms {
        eprintln!("stage {stage:<7} p50 {ms:.1} ms");
    }

    let key = probe.host_key();
    match Baseline::load(&repo_root(), &key) {
        None => {
            eprintln!("\nno baseline for host {key} — reporting only.");
            if std::env::var("DS_ITEST_WRITE_BASELINE").is_ok() {
                let p = now.save(&repo_root())?;
                eprintln!("wrote {}", p.display());
            } else {
                eprintln!("set DS_ITEST_WRITE_BASELINE=1 to record this run as the baseline for this host.");
            }
        }
        Some(base) => {
            eprintln!("\n── against baseline {key} ({} documents) ──", base.documents);
            let (comparisons, clamped) = latency::compare_with(&base, &now, TOLERANCE);
            if let Some(raw) = clamped {
                eprintln!(
                    "note: the host-speed factor was x{raw:.2} and is clamped to x{:.2}. One of the two CPU probes \
                     was taken on a busy machine, so these ratios carry that uncertainty.",
                    latency::MAX_CPU_FACTOR
                );
            }
            for c in &comparisons {
                eprintln!(
                    "{:<18} {:>8.1} → {:>8.1} ms   normalised x{:.2} (tol {:.2}) {}",
                    c.name, c.baseline, c.measured, c.normalised_ratio, c.tolerance,
                    if c.within() { "ok" } else { "REGRESSED" }
                );
            }
            // The share assertion is the one that catches work added to the hot path,
            // which no timing can tell apart from a slower model tier.
            let share_drift = (now.model_share - base.model_share).abs();
            eprintln!("model share {:.2} → {:.2} (drift {:.2})", base.model_share, now.model_share, share_drift);

            // A run that looks much faster is reported, never celebrated: with the
            // non-degeneracy checks above already passed, the likeliest explanation is a
            // baseline recorded on a cold stack rather than an improvement.
            let fast: Vec<&str> = comparisons.iter().filter(|c| c.suspiciously_fast()).map(|c| c.name.as_str()).collect();
            if !fast.is_empty() {
                eprintln!(
                    "note: {fast:?} came out below x{:.2} of the baseline. The work was done (pages ran, no failures), \
                     so this is more likely a stale baseline than a speed-up — re-record with DS_ITEST_WRITE_BASELINE=1.",
                    latency::SUSPICIOUSLY_FAST
                );
            }
            if now.documents < latency::MIN_DOCUMENTS_FOR_TAIL {
                eprintln!(
                    "note: {} documents is below {}, so the tail is reported and not compared.",
                    now.documents,
                    latency::MIN_DOCUMENTS_FOR_TAIL
                );
            }

            let regressed: Vec<&str> = comparisons.iter().filter(|c| !c.within()).map(|c| c.name.as_str()).collect();
            assert!(regressed.is_empty(), "slower than this host's baseline: {regressed:?}");
            assert!(share_drift <= 0.25, "the model's share of a document moved by {share_drift:.2}, so the work changed shape");
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
