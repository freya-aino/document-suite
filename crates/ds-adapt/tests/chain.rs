//! The whole adaptation chain, once, on a corpus generated here.
//!
//! `ds-adapt`'s status said it plainly: *"It has never been run end to end on a real
//! dataset. Every metric is unit-tested against synthetic inputs, so the machinery is
//! proven but its outputs are not calibrated."* The second sentence is still true and
//! cannot be made false without real documents and real models. The first was true for a
//! different reason — there was no corpus to run on and no test that ran the five
//! commands as a chain — and this is that reason removed.
//!
//! What this proves, exactly: `dataset → prepare → run → score → select → report`
//! executes on labelled documents, through the real step registry, the real executor, the
//! real metrics and the real selection, and produces a routed DAG list and a report
//! consistent with it.
//!
//! What it does not prove: anything about document parsing. The model tier is a fake
//! server answering the same four lines to every page — so the *quality* numbers measure
//! that fake, and a DAG winning here has won on this corpus and nothing else. The gap
//! between these two paragraphs is what a real dataset and a real model tier close, and
//! `SPEC.md` keeps it stated.
//!
//! Needs `PDFIUM_DYNAMIC_LIB_PATH` (devenv sets it) and renders pages, so it is
//! `#[ignore]`d with the rest of the slow set.

use std::sync::Arc;
use std::time::Duration;

use ds_adapt::dataset::{self, Spec};
use ds_adapt::records::read_jsonl;
use ds_adapt::run::{LocalFetcher, ManifestEntry, RunConfig, Runner};
use ds_adapt::score::{score as score_runs, MetricWeights, ScoreOptions};
use ds_adapt::select::{select, Objective};
use ds_adapt::{report, RunRecord};

// The fake model tier is `ds_testkit::models` — in-process and on a real socket, for the
// same reason the suite's own pipeline test does it that way: a fake *client* would skip
// the transport, the registry's health ejection and the parser, which is most of what the
// run path is.
//
// It used to be a hand-rolled copy here, one of three in the workspace, each with its own
// idea of what a model answers. A fake that disagrees with itself across three files can
// make two tests pass for contradictory reasons.

/// Whether pdfium can be bound at all. Without it nothing here can render a page, and
/// the test says so and returns rather than failing — the same convention
/// `ds-ingest`'s own tests use, so a machine without pdfium gets a skip and a machine
/// with it gets a real failure.
fn pdfium_or_skip() -> bool {
    if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_some() {
        return true;
    }
    eprintln!("skipping: PDFIUM_DYNAMIC_LIB_PATH is unset, so no page can be rendered");
    false
}

/// The render child, next to the test binary.
fn render_child() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    // target/debug/deps/chain-<hash> → target/debug/ds-render-child
    let dir = exe.parent().and_then(|d| d.parent()).expect("target/debug");
    let path = dir.join("ds-render-child");
    assert!(path.exists(), "{} is missing; build the workspace before running this test", path.display());
    path
}

fn registry_toml(endpoint: &str) -> String {
    format!(
        r#"
[[model]]
id = "fake-parse"
revision = "t1"
family = "generic_json"
task = ["page_parse", "layout", "region_ocr", "table", "formula", "figure_describe", "chart_to_table"]
endpoints = ["{endpoint}"]
max_concurrency = 4
timeout_ms = 20000
languages = ["*"]
"#
    )
}

/// Two candidates that differ in shape, so the selection has something to choose between:
/// one model call for the page, against layout + per-region recognition merged with the
/// text layer.
fn candidates() -> Vec<ds_dag::Dag> {
    serde_json::from_value(serde_json::json!([
        {
            "id": "e2e", "version": "1", "output": "parse", "description": "one call for the page",
            "nodes": [{ "id": "parse", "step": "page.parse_e2e", "inputs": ["$page_image"],
                        "params": { "model": "fake-parse" }, "timeout_ms": null, "optional": false }]
        },
        {
            "id": "layout_then_ocr", "version": "1", "output": "merge", "description": "layout, recognise, merge with the text layer",
            "nodes": [
                { "id": "layout", "step": "layout.detect", "inputs": ["$page_image"],
                  "params": { "model": "fake-parse" }, "timeout_ms": null, "optional": false },
                { "id": "ocr", "step": "region.ocr", "inputs": ["$page_image", "layout"],
                  "params": { "model": "fake-parse" }, "timeout_ms": null, "optional": false },
                { "id": "merge", "step": "blocks.merge_native", "inputs": ["$text_layer", "layout", "ocr"],
                  "params": null, "timeout_ms": null, "optional": false }
            ]
        }
    ]))
    .expect("candidate DAGs")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "renders pages; needs PDFIUM_DYNAMIC_LIB_PATH from devenv"]
async fn the_whole_chain_runs_on_a_generated_corpus() {
    if !pdfium_or_skip() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("ds-adapt-chain-{}", ds_core::RunId::new()));
    let corpus = dir.join("corpus");
    let out = dir.join("run");
    std::fs::create_dir_all(&out).expect("out");

    // ── dataset ──
    // Two documents per shape and half of them held out, so the chain exercises the one
    // thing that makes any of its numbers quotable: a selection made on `train` and
    // measured on `holdout`. At one document per shape `round(1 * share)` is zero and
    // there is no holdout to confirm on, which is correct and useless here.
    let spec = Spec { docs_per_shape: 2, pages_per_doc: 1, holdout_share: 0.5, ..Default::default() };
    let written = dataset::write(&corpus, &spec).expect("corpus");
    assert_eq!(written.documents, 8, "two documents of each of the four page shapes");
    let manifest: Vec<ManifestEntry> = read_jsonl(&written.manifest).expect("manifest");

    // ── run ──
    let (endpoint, server) = ds_testkit::models::spawn().await;
    let client = Arc::new(ds_models::OpenAiClient::new(ds_models::ModelRegistry::from_toml(&registry_toml(&endpoint)).expect("registry")));
    let registry = ds_steps::production_registry(client.clone());
    let dags = candidates();
    for d in &dags {
        d.validate(&registry).unwrap_or_else(|e| panic!("candidate {}: {e}", d.id));
    }
    let pool = Arc::new(ds_ingest::RenderPool::new(
        // `ds-render-child` belongs to another crate, so Cargo does not export its path
        // to this test: it is found next to the test binary, where a workspace build puts
        // every binary.
        ds_ingest::ChildCommand { program: render_child(), args: vec![], env: vec![] },
        ds_ingest::PoolConfig { size: 2, page_timeout: Duration::from_secs(60), open_timeout: Duration::from_secs(60) },
    ));
    let classify_cfg = ds_classify::ClassifyConfig::default();
    let objective = Objective {
        // The fake tier answers the same three lines for every page, so quality measures
        // the fake and the floor has to be below it or nothing is selected. That is the
        // honest shape of this test: the chain is under test, the numbers are not.
        quality_floor: 0.01,
        min_pages: 1,
        ..Default::default()
    };
    let runner = Runner {
        registry,
        pool,
        classifier: None, // no vision model: the rule tier classifies, as on a small deployment
        classify_cfg: classify_cfg.clone(),
        fetcher: Box::new(LocalFetcher),
        cfg: RunConfig {
            out: out.clone(),
            sample: 2,
            rounds: 1,
            max_pages: 4,
            dpi: 150,
            thumb_px: classify_cfg.thumbnail_long_side_px,
            parallel: 2,
            split: None,
            keep_share: 1.0,
            objective: objective.clone(),
            weights: MetricWeights::default(),
            ignore_doc_type_hint: false,
        },
    };

    let pages = runner.prepare(&manifest).await.expect("prepare");
    assert_eq!(pages.len(), 8, "one page per document");
    assert!(pages.iter().all(|p| p.ground_truth.as_ref().is_some_and(|gt| !gt.is_empty())), "every page must arrive labelled");
    assert!(pages.iter().all(|p| p.profile.born_digital), "the generated PDFs carry a text layer");

    let records: Vec<RunRecord> = runner.run_all(&dags, &pages).await.expect("run");
    assert!(records.iter().all(|r| r.ok), "a candidate failed: {:?}", records.iter().find(|r| !r.ok).map(|r| &r.error));
    assert!(records.len() >= pages.len(), "every surviving candidate runs on every page of the final round");

    // ── score ──
    let evals = score_runs(records.clone(), &ScoreOptions { base: Some(&out), weights: MetricWeights::default(), judge: None })
        .await
        .expect("score");
    assert_eq!(evals.len(), records.len());
    assert!(evals.iter().all(|e| e.trust == ds_adapt::Trust::Labelled), "the corpus is labelled, so nothing needs a judge");
    assert!(evals.iter().any(|e| e.quality > 0.0), "every page scored zero, which means the metrics saw nothing");
    // The parse path reached the eval, which is what makes it reportable.
    assert!(
        evals.iter().any(|e| e.parse_paths.iter().any(|p| p == "generic_json")),
        "the node metrics did not reach the eval: {:?}",
        evals.first().map(|e| &e.parse_paths)
    );

    // ── select ──
    let list = select(&evals, dags.clone(), &objective, "chain-test-1", Some("tests/chain.rs".into())).expect("a DAG list");
    assert!(!list.dags.is_empty());
    assert!(list.dags.iter().any(|d| d.id == list.routing.default_dag), "the default DAG must be in the list it belongs to");
    // The list is valid against the production registry — the same check the API's
    // upload route runs, so a list this produces is one a deployment can accept.
    let client2 = Arc::new(ds_models::OpenAiClient::new(ds_models::ModelRegistry::from_toml(&registry_toml(&endpoint)).expect("registry")));
    ds_router::Router::load(list.clone(), &ds_steps::production_registry(client2)).expect("the selected list must be loadable");

    // ── report ──
    let md = report::render(&list, &evals, &objective, None);
    assert!(md.contains("# DAG list `chain-test-1`"));
    assert!(md.contains("## Parse paths"), "the report must say how the output was parsed");
    assert!(md.contains("Pages evaluated: **8**"), "{md}");
    // The size of the thing these numbers were measured on, printed beside them: a
    // single winner out of C candidates on n pages implies a discrimination this corpus
    // may not contain, and the report says so with the actual C and n.
    assert!(md.contains("Selection bias"), "the report must state the winner's curse: {md}");
    assert!(md.contains("Documents:"), "{md}");
    // Where the time went, from the executor's own levels. The fake answers in
    // milliseconds, so the numbers are small — the claim is that the attribution exists
    // and that host and model are told apart, not that either is large.
    assert!(md.contains("## Latency attribution"), "{md}");
    assert!(evals.iter().any(|e| e.model_ms > 0), "no eval attributed any time to the model tier");

    // ── the confirmation pass ──
    //
    // The same list, reported over the split it was *not* selected on. This is what
    // `ds-adapt report --split holdout` does, and the only report whose numbers are
    // quotable: selection on n pages out of C candidates inflates the winner.
    let holdout: Vec<_> = evals.iter().filter(|e| e.split.as_deref() == Some("holdout")).cloned().collect();
    assert!(!holdout.is_empty(), "the corpus must have a holdout split for a confirmation pass to be possible");
    let confirmation = report::render(&list, &holdout, &objective, None);
    assert!(confirmation.contains("holdout"), "the confirmation report must say which split it is: {confirmation}");

    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}

/// Scoring the classifier, on the same generated corpus.
///
/// Two gaps met here. The classifier was never scored at all — everything else measures
/// DAG *output*, not the thing that chooses the DAG — and its confidence was never
/// calibrated, while the routing table thresholds on exactly that number.
///
/// The first run of this against a corpus found something: with no vision model, the rule
/// tier has no document-type opinion, and it used to report `rules_only_confidence`
/// (0.85) for a type of `other` that nothing had looked for. Every such page therefore
/// sailed past `min_classifier_confidence`, which exists to catch precisely that page.
/// The expected calibration error measured it at 85 %; it is a third of that now, and the
/// remaining error is the honest residue of a tier that does not classify document type.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "renders pages; needs PDFIUM_DYNAMIC_LIB_PATH from devenv"]
async fn the_classifier_is_scored_against_the_corpus_labels() {
    if !pdfium_or_skip() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("ds-adapt-classify-{}", ds_core::RunId::new()));
    let corpus = dir.join("corpus");
    let out = dir.join("run");
    std::fs::create_dir_all(&out).expect("out");
    let spec = Spec { docs_per_shape: 1, pages_per_doc: 1, holdout_share: 0.0, ..Default::default() };
    let written = dataset::write(&corpus, &spec).expect("corpus");
    let manifest: Vec<ManifestEntry> = read_jsonl(&written.manifest).expect("manifest");

    let client = Arc::new(ds_models::OpenAiClient::new(
        ds_models::ModelRegistry::from_toml(&registry_toml("http://127.0.0.1:1/v1")).expect("registry"),
    ));
    let classify_cfg = ds_classify::ClassifyConfig::default();
    let runner = Runner {
        registry: ds_steps::production_registry(client),
        pool: Arc::new(ds_ingest::RenderPool::new(
            ds_ingest::ChildCommand { program: render_child(), args: vec![], env: vec![] },
            ds_ingest::PoolConfig { size: 2, page_timeout: Duration::from_secs(60), open_timeout: Duration::from_secs(60) },
        )),
        classifier: None,
        classify_cfg: classify_cfg.clone(),
        fetcher: Box::new(LocalFetcher),
        cfg: RunConfig {
            out,
            sample: 1,
            rounds: 1,
            max_pages: 2,
            dpi: 150,
            thumb_px: classify_cfg.thumbnail_long_side_px,
            parallel: 2,
            split: None,
            keep_share: 1.0,
            objective: Objective::default(),
            weights: MetricWeights::default(),
            // The hint *is* the label: a classifier handed it would score perfectly and
            // say nothing.
            ignore_doc_type_hint: true,
        },
    };
    let pages = runner.prepare(&manifest).await.expect("prepare");
    assert_eq!(pages.len(), 4);

    let judged: Vec<ds_adapt::Judged> = pages
        .iter()
        .filter_map(|p| {
            // The page carries its manifest index, so the label comes from the entry that
            // produced it rather than from a second guess at the document id.
            let label = manifest.get(p.manifest_index)?.doc_type_hint?;
            Some(ds_adapt::Judged { page_id: format!("{}/{}", p.doc, p.page_no), label, profile: p.profile.clone() })
        })
        .collect();
    assert_eq!(judged.len(), 4, "every generated document carries a label");

    let score = ds_adapt::score_classifier(&judged);
    assert_eq!(score.pages, 4);
    assert_eq!(score.rules_only, 4, "no vision model was configured, so this scores the rule tier");
    // The rule tier does not determine a document type from a born-digital page, and now
    // says so instead of claiming 0.85: below the routing table's default threshold, so
    // these pages take the low-confidence DAG, which is what it is for.
    assert!(
        judged.iter().all(|j| j.profile.confidence < Objective::default().min_classifier_confidence),
        "a page with no determined type must fall below the threshold meant to catch it: {:?}",
        judged.iter().map(|j| j.profile.confidence).collect::<Vec<_>>()
    );
    let err = score.calibration_error().expect("pages were scored");
    assert!(err < 0.5, "expected calibration error was 85 % before the rules tier stopped overstating itself, got {err}");

    let md = ds_adapt::classify_score::render(&score);
    assert!(md.contains("## Page classifier"));
    assert!(md.contains("Expected calibration error"));
    assert!(md.contains("### Confusions"), "the report must say which way it was wrong: {md}");
    std::fs::remove_dir_all(&dir).ok();
}
