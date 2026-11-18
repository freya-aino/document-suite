//! One document through the whole pipeline: ingest → Stage 1 per page → Stage 2 →
//! Postgres + Qdrant.
//!
//! This is the test every checklist in this repo has been pointing at. It drives
//! `Pipeline::run` — the same call the service makes for an accepted upload — so the
//! handover between stages is the real one and not a fake on either side of it.
//!
//! What is real here: the blob store, Postgres, Qdrant, the pipeline exactly as
//! `build_pipeline` wires it for a deployment, rasterisation, classification, routing,
//! DAG execution, stitching, extraction, grounding, verification, corpus resolution, the
//! vault's write path and the vector writes.
//!
//! What is substituted, and why:
//!
//! - **The models.** A real VLM would make the assertions non-deterministic, and a test
//!   that cannot say what the output should be is not a test. The fake server speaks the
//!   OpenAI-compatible wire protocol over a real socket through the real `OpenAiClient`,
//!   and answers *adaptively* — it reads the block labels out of the prompt it was sent
//!   and quotes that block's own text back. That matters: a canned answer would be
//!   dropped by grounding, so this is the only shape of fake that exercises the grounding
//!   path rather than skipping it.
//!
//! And that is all that is substituted. There used to be a second entry here for the
//! task bus, an in-process queue standing in for JetStream while the stages were three
//! processes. The bus is gone, so the stages are function calls and there is nothing
//! left to fake between them.
//!
//! `cargo test -p vs-suite -- --ignored`, with `devenv up` or the `vs-test-*` containers.

use std::collections::HashMap;

use serde_json::{json, Value};
use sqlx::PgPool;

use vs_core::{HashKey, JobSpec, JobState, LanguageTag, Priority, Profile, RunId, TenantId};
use vs_infra::{BlobStore, Config, ObjectBlobStore};
use vs_vault::ops::Ops;
use vs_worker::{build_pipeline, Deps};

const DIMS: usize = 8;
const TENANT: &str = "t_pipeline";

// ───────────────────────── the fake model tier ─────────────────────────

/// Serves the three OpenAI-compatible endpoints the pipeline calls, dispatching on the
/// request's `model` field rather than on the prompt's content — a prompt is a brittle
/// thing to match on, a model id is the registry's own name for a job.
async fn fake_models() -> u16 {
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat))
        .route("/v1/embeddings", axum::routing::post(embeddings));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    port
}

fn completion(content: Value) -> Value {
    json!({ "choices": [{ "message": { "content": content.to_string() } }], "usage": { "prompt_tokens": 10, "completion_tokens": 20 } })
}

async fn chat(axum::Json(req): axum::Json<Value>) -> axum::Json<Value> {
    let model = req["model"].as_str().unwrap_or_default().to_string();
    let text: String = req["messages"]
        .as_array()
        .map(|ms| {
            ms.iter()
                .filter_map(|m| match &m["content"] {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(parts) => Some(parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let body = match model.as_str() {
        // Counted, so the two pages read differently. Identical pages would give the
        // extractor two identical windows, and two identical claims collapse to one row
        // by content hash — true of the store and a poor thing to build a count on.
        "parse-vlm" => {
            static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            completion(json!({
                "blocks": [
                    { "kind": "title", "bbox": [0.1, 0.05, 0.9, 0.1], "text": format!("Section {n}: retrieval quality") },
                    { "kind": "text", "bbox": [0.1, 0.15, 0.9, 0.25], "text": format!("Supplier{n} Holdings reported a recall of 0.9{n} on the invoice set.") }
                ]
            }))
        }
        "extract-llm" => completion(extraction(&text)),
        "verify-llm" => completion(verdicts(&text)),
        other => panic!("the pipeline called an unexpected model {other:?}"),
    };
    axum::Json(body)
}

/// Builds an extraction that *grounds*: the quote is copied out of the block the prompt
/// showed, so `parse::ground_output` keeps it. Returning a plausible-looking constant
/// instead would be silently dropped and the test would pass while asserting nothing.
fn extraction(prompt: &str) -> Value {
    let Some((label, body)) = first_block(prompt) else {
        return json!({ "entities": [], "assertions": [] });
    };
    // A quote has to be verbatim, so take a real substring of the block.
    let quote: String = body.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
    let name = body.split_whitespace().next().unwrap_or("Thing").trim_end_matches(&[',', '.'][..]).to_string();
    json!({
        "entities": [{
            "id": "e1",
            "name": name,
            "type": "organisation",
            "mentions": [{ "block": label, "quote": quote }],
        }],
        "assertions": [
            {
                "predicate": "mentions_metric",
                "kind": "attribute",
                "args": [
                    { "role": "subject", "entity": "e1", "literal": null },
                    { "role": "value", "entity": null, "literal": { "value": quote.clone(), "unit": null } },
                ],
                "qualifiers": { "negated": false, "hedged": false, "condition": null, "time": null },
                "evidence": [{ "block": label, "quote": quote }],
                "confidence": 0.9,
            },
            // Deliberately identical in every window: same predicate, same literal, no
            // entity arg. The store is content-addressed, so the two windows' copies must
            // land as one row — the property that made the first draft of this test's
            // counts wrong, and worth pinning now that it is understood.
            {
                "predicate": "document_is_about",
                "kind": "attribute",
                "args": [{ "role": "value", "entity": null, "literal": { "value": "invoices", "unit": null } }],
                "qualifiers": { "negated": false, "hedged": false, "condition": null, "time": null },
                "evidence": [{ "block": label, "quote": quote }],
                "confidence": 0.7,
            },
        ],
    })
}

/// `[B1] the block's text` — the format `vs_extract::window::render_window` produces.
///
/// Scanned, not read off the first line: the rendered window opens with `Document:` and
/// `Section:` headers, so anything that insists on a label at line 1 finds nothing.
fn first_block(prompt: &str) -> Option<(String, String)> {
    prompt.lines().find_map(|line| {
        let rest = line.trim().strip_prefix('[')?;
        let (label, body) = rest.split_once("] ")?;
        (label.starts_with('B') && !body.trim().is_empty()).then(|| (label.to_string(), body.trim().to_string()))
    })
}

/// Supports every assertion it is shown. The ids come out of the prompt, which is the
/// whole contract: a verdict for an id the extractor did not emit is ignored, so a
/// hard-coded id would silently verify nothing.
fn verdicts(prompt: &str) -> Value {
    let ids: Vec<&str> = prompt.lines().filter_map(|l| l.trim().strip_prefix("- id: ")).map(str::trim).collect();
    json!({ "verdicts": ids.iter().map(|id| json!({ "id": id, "supported": true })).collect::<Vec<_>>() })
}

async fn embeddings(axum::Json(req): axum::Json<Value>) -> axum::Json<Value> {
    let n = match &req["input"] {
        Value::Array(a) => a.len(),
        _ => 1,
    };
    // Distinct vectors, so a mix-up between two embed targets is visible rather than
    // hidden behind identical points.
    let data: Vec<Value> = (0..n)
        .map(|i| {
            let v: Vec<f32> = (0..DIMS).map(|d| ((i + d) % 7) as f32 / 7.0 + 0.01).collect();
            json!({ "embedding": v, "index": i })
        })
        .collect();
    axum::Json(json!({ "data": data }))
}

/// A registry naming the three fakes, written next to the config so `[models].registry`
/// can point at it.
fn registry_toml(port: u16) -> String {
    let ep = format!("http://127.0.0.1:{port}/v1");
    format!(
        r#"
[[model]]
id = "parse-vlm"
revision = "test"
family = "generic_json"
task = ["page_parse"]
endpoints = ["{ep}"]
coord_scale = "fractions"

[[model]]
id = "extract-llm"
revision = "test"
family = "generic_json"
task = ["extract"]
endpoints = ["{ep}"]

[[model]]
id = "verify-llm"
revision = "test"
family = "generic_json"
task = ["verify"]
endpoints = ["{ep}"]

[[model]]
id = "embed-text"
revision = "test"
family = "embedding"
task = ["embed"]
endpoints = ["{ep}"]
dimensions = {DIMS}
batch_size = 16
"#
    )
}

// ───────────────────────── the in-process bus ─────────────────────────

// ───────────────────────── the document ─────────────────────────

/// A two-page TIFF, so the `doc` role's fan-in is exercised: the `doc` task must be
/// published once, after the last page, and not once per page.
///
/// A raster rather than a PDF because a TIFF needs no pdfium, and because a scan takes the
/// recognition path — a born-digital page would be read from its text layer and never call
/// a model at all, which is the half of Stage 1 worth testing here.
fn two_page_tiff() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut enc = tiff::encoder::TiffEncoder::new(&mut buf).expect("tiff encoder");
    for page in 0..2u32 {
        let (w, h) = (600u32, 800u32);
        let mut px = vec![255u8; (w * h) as usize];
        // A few dark bands, so ink coverage and the orientation profile see something.
        for row in 0..6 {
            let y0 = 100 + row * 40 + page * 5;
            for y in y0..y0 + 12 {
                for x in 60..540 {
                    px[(y * w + x) as usize] = 20;
                }
            }
        }
        enc.write_image::<tiff::encoder::colortype::Gray8>(w, h, &px).expect("write tiff page");
    }
    buf.into_inner()
}

/// A DAG list the `page` role will actually use: one VLM call per page.
fn dag_list(version: &str) -> Value {
    json!({
        "version": version,
        "dags": [{
            "id": "e2e", "version": "1", "output": "parse",
            "nodes": [{ "id": "parse", "step": "page.parse_e2e", "inputs": ["$page_image"], "params": { "model": "parse-vlm" }, "timeout_ms": null }],
        }],
        "routing": { "default_dag": "e2e", "low_confidence_dag": "e2e", "min_classifier_confidence": 0.0, "rules": [] },
    })
}

fn spec(source_uri: String) -> JobSpec {
    JobSpec {
        tenant: TenantId(TENANT.into()),
        language: LanguageTag::parse("en").unwrap(),
        priority: Priority::Batch,
        profile: Profile::Balanced,
        dag_list_version: None,
        source_uri,
        external_ref: Some("pipeline-test".into()),
        doc_type: None,
        // A real owner, so the caller the API verified travels the whole way: the spec is
        // re-serialized onto the bus between all three roles and into `ops.jobs.spec`.
        owner: Some(vs_core::Owner {
            subject: "person-1".into(),
            department: Some("dept-a".into()),
            workspaces: vec!["ws-red".into()],
        }),
    }
}

// ───────────────────────── the test ─────────────────────────

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres, Qdrant and an S3 blob store"]
async fn a_document_goes_from_upload_to_both_stores(pool: PgPool) {
    // `RUST_LOG=vs_extract=debug cargo test -p vs-suite --test pipeline -- --ignored
    // --nocapture` is how you find out which stage dropped something, which is the
    // question this test raises when it fails.
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let port = fake_models().await;
    let dir = std::env::temp_dir().join(format!("vs-pipeline-{}", RunId::new()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let registry_path = dir.join("models.toml");
    std::fs::write(&registry_path, registry_toml(port)).expect("write registry");
    let prefix = format!("itest_{}", RunId::new().to_string().replace('-', ""));
    let config_path = dir.join("worker.toml");
    std::fs::write(&config_path, worker_toml(&registry_path, &prefix, &dir)).expect("write config");
    let cfg = Config::load(&config_path).expect("load config");

    // The original, where `vs-api` would have put it.
    let blob = ObjectBlobStore::s3(&cfg.blob).expect("blob store");
    let source_uri = format!("originals/{TENANT}/pipeline.tiff");
    blob.put(&source_uri, two_page_tiff().into(), "image/tiff").await.expect("upload the original");

    let ops = Ops { pool: pool.clone() };
    ops.dag_list_insert_candidate("pipeline-1", None, &serde_json::from_value(dag_list("pipeline-1")).expect("dag list"), Some("pipeline-test"))
        .await
        .expect("insert dag list");
    assert!(ops.dag_list_activate("pipeline-1", None).await.expect("activate"), "the dag list must activate");

    let spec = spec(source_uri);
    // v7, like the API's own ids: the workspace does not enable uuid's v4 feature.
    let (job_id, run_id) = (uuid::Uuid::now_v7(), RunId::new());
    ops.create_job(job_id, None, run_id, &spec).await.expect("create job");

    // One call, which is the point: ingest, both pages concurrently, then Stage 2. What
    // used to be three handler invocations with a queue between them is now the same
    // function the service calls when it accepts an upload.
    let pipeline = pipeline(&cfg, pool.clone()).await;
    pipeline.run(job_id, run_id, &spec).await.unwrap_or_else(|e| panic!("pipeline failed: {:#}", e.error));

    // The key the stores are addressed by. Taken from the content rather than from a
    // message, now that there are no messages.
    let document_hk = HashKey::document(&TenantId(TENANT.into()), &vs_core::content_sha256(&two_page_tiff()));

    // ── what the stores hold ──
    let tenant = TenantId(TENANT.into());
    let job = ops.get_job(&tenant, job_id).await.expect("get job").expect("the job row");
    // `done`, not `partial`: `max_failed_page_fraction = 0.0` means a single failed page
    // would have failed the document, so this also asserts both pages succeeded.
    assert_eq!(job.state, vs_vault::enum_name(&JobState::Done), "job ended {:?}: {:?}", job.state, job.error);
    assert_eq!(job.pages_total, Some(2));
    assert_eq!(job.pages_done, 2);
    assert_eq!(job.pages_failed, 0);

    // Exact counts, not lower bounds: the fake parser returns two blocks per page, so
    // everything downstream of it is determined. A lower bound here would pass while the
    // second page quietly went missing.
    let counts = row_counts(&pool, &document_hk, run_id).await;
    assert_eq!(counts["hub_document"], 1, "{counts:#?}");
    assert_eq!(counts["hub_page"], 2, "{counts:#?}");
    assert_eq!(counts["lnk_document_page"], 2, "{counts:#?}");
    assert_eq!(counts["hub_block"], 4, "two blocks per page:\n{counts:#?}");
    assert_eq!(counts["sat_block_content"], 4, "every block needs its content:\n{counts:#?}");
    // Every block must be reachable from the document, or a citation cannot be resolved,
    // which is what the vault layer exists for.
    assert_eq!(counts["lnk_page_block"], 4, "{counts:#?}");

    // Two windows × two assertions = four extracted, but one of the two is identical in
    // both windows and the store is content-addressed, so three rows. This is the
    // insert-only vault behaving as designed, and nothing else in the suite tests it
    // across two separately extracted windows.
    assert_eq!(counts["hub_assertion"], 3, "{counts:#?}");
    assert_eq!(counts["hub_entity"], 2, "one entity per window, resolved apart:\n{counts:#?}");
    // The write path that produced these failed for months on a NULL `assertion_hk`, so
    // assert the links and not only the hub.
    //
    // Two arg links, not three: `lnk_assertion_arg` carries `entity_hk NOT NULL`, so it
    // holds only the args that point at an entity. `document_is_about` has a literal and
    // nothing else, and its value lives in `sat_assertion_detail.literals`. A reader
    // looking for an assertion's arguments has to read both.
    assert_eq!(counts["lnk_assertion_arg"], 2, "{counts:#?}");
    assert_eq!(counts["lnk_assertion_evidence"], 3, "every assertion must be citable:\n{counts:#?}");
    // The verify model supported everything it was shown, so the flag must have come back
    // through `parse_verdicts` and reached the satellite. Nothing else tests that round
    // trip: it is the one place a model's answer changes a stored row.
    let verified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM vault.sat_assertion_detail d JOIN vault.hub_assertion h ON h.assertion_hk = d.assertion_hk \
         WHERE h.run_id = $1 AND d.verified",
    )
    .bind(run_id.0)
    .fetch_one(&pool)
    .await
    .expect("counting verified assertions");
    assert_eq!(verified, 3, "every assertion was supported by the verify model");

    // Per collection, not a total: a regression that wrote the blocks and dropped the
    // entities would pass a total.
    let points = qdrant_points(&cfg, &prefix).await;
    assert_eq!(points["blocks"], 4, "one point per block with text: {points:?}");
    assert!(points["sections"] >= 1, "the stitched sections were not indexed: {points:?}");
    assert!(points["entities"] >= 1, "the resolved entities were not indexed: {points:?}");

    std::fs::remove_dir_all(&dir).ok();
}

async fn pipeline(cfg: &Config, pool: PgPool) -> vs_worker::Pipeline {
    let render_command = vs_ingest::ChildCommand {
        // The built service binary, because a test's own `current_exe` is the test binary
        // and does not speak the render-child protocol. Cargo sets this variable for an
        // integration test and guarantees the binary exists, which is stronger than
        // deriving the path from `current_exe`. A TIFF decodes in-process and never
        // spawns a child, so this only matters once the fixture becomes a PDF.
        program: env!("CARGO_BIN_EXE_vs-suite").into(),
        args: vec!["render-child".into()],
        env: vec![],
    };
    build_pipeline(cfg, Deps { pool, render_command })
        .await
        .unwrap_or_else(|e| panic!("could not build the pipeline: {e:#}"))
}

fn worker_toml(registry: &std::path::Path, qdrant_prefix: &str, scratch: &std::path::Path) -> String {
    let endpoint = std::env::var("AWS_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:3900".into());
    let qdrant = std::env::var("VS__QDRANT__URL").unwrap_or_else(|_| "http://127.0.0.1:6334".into());
    let bucket = std::env::var("VS_TEST_BUCKET").unwrap_or_else(|_| "visual-suite".into());
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "garage".into());
    format!(
        r#"

[blob]
bucket = "{bucket}"
endpoint = "{endpoint}"
region = "{region}"

[postgres]
# Unused: the handlers take the pool `#[sqlx::test]` created, which is a fresh migrated
# database per test rather than the shared one this URL names.
url = "postgres://vs@127.0.0.1:5432/postgres"
max_connections = 4

[qdrant]
url = "{qdrant}"
collection_prefix = "{qdrant_prefix}"

[models]
registry = "{registry}"

[worker]
max_in_flight = 4
interactive_share = 0.3
render_dpi = 150
max_failed_page_fraction = 0.0
scratch_dir = "{scratch}"
render_processes = 1
page_render_timeout_s = 60

[classify]
born_digital_min_chars = 50
born_digital_min_coverage = 0.8

[stage2]
extraction_model = "extract-llm"
verify_model = "verify-llm"
embedding_model = "embed-text"
window_chars = 12000
embed_batch_size = 0
dag_list_refresh_s = 1

[telemetry]
json_logs = false
"#,
        registry = registry.display(),
        scratch = scratch.display(),
    )
}

/// Row counts in the vault for this document, by table.
async fn row_counts(pool: &PgPool, document_hk: &HashKey, run_id: RunId) -> HashMap<String, i64> {
    let doc = document_hk.as_str();
    let mut out = HashMap::new();
    let scoped: &[(&str, &str)] = &[
        ("hub_document", "SELECT count(*) FROM vault.hub_document WHERE document_hk = $1"),
        ("hub_page", "SELECT count(*) FROM vault.hub_page p JOIN vault.lnk_document_page l ON l.page_hk = p.page_hk WHERE l.document_hk = $1"),
        ("lnk_document_page", "SELECT count(*) FROM vault.lnk_document_page WHERE document_hk = $1"),
        (
            // `hub_block` carries its own `page_hk`, so joining through `lnk_page_block`
            // here would be ambiguous as well as redundant.
            "hub_block",
            "SELECT count(*) FROM vault.hub_block b JOIN vault.lnk_document_page dp ON dp.page_hk = b.page_hk WHERE dp.document_hk = $1",
        ),
        (
            "lnk_page_block",
            "SELECT count(*) FROM vault.lnk_page_block pb JOIN vault.lnk_document_page dp ON dp.page_hk = pb.page_hk WHERE dp.document_hk = $1",
        ),
        (
            "sat_block_content",
            "SELECT count(*) FROM vault.sat_block_content s JOIN vault.hub_block b ON b.block_hk = s.block_hk \
             JOIN vault.lnk_document_page dp ON dp.page_hk = b.page_hk WHERE dp.document_hk = $1",
        ),
    ];
    for (name, sql) in scoped {
        let n: i64 = sqlx::query_scalar(*sql).bind(doc).fetch_one(pool).await.unwrap_or_else(|e| panic!("counting {name}: {e}"));
        out.insert((*name).to_string(), n);
    }

    // Assertions are scoped by `run_id`, not by document — `hub_assertion` has no
    // `document_hk` column, which is also how `docs/STORES.md` says to query them.
    let by_run: &[(&str, &str)] = &[
        ("hub_assertion", "SELECT count(*) FROM vault.hub_assertion WHERE run_id = $1"),
        (
            "lnk_assertion_arg",
            "SELECT count(*) FROM vault.lnk_assertion_arg a JOIN vault.hub_assertion h ON h.assertion_hk = a.assertion_hk WHERE h.run_id = $1",
        ),
        (
            "lnk_assertion_evidence",
            "SELECT count(*) FROM vault.lnk_assertion_evidence e JOIN vault.hub_assertion h ON h.assertion_hk = e.assertion_hk WHERE h.run_id = $1",
        ),
    ];
    for (name, sql) in by_run {
        let n: i64 = sqlx::query_scalar(*sql).bind(run_id.0).fetch_one(pool).await.unwrap_or_else(|e| panic!("counting {name}: {e}"));
        out.insert((*name).to_string(), n);
    }
    // Entities are corpus-scoped, not document-scoped: they outlive the run by design.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM vault.hub_entity WHERE tenant_id = $1")
        .bind(TENANT)
        .fetch_one(pool)
        .await
        .expect("counting hub_entity");
    out.insert("hub_entity".into(), n);
    out
}

/// Points per collection, read over Qdrant's REST API so the test does not use the
/// writer's own client to confirm the writer's work.
async fn qdrant_points(cfg: &Config, prefix: &str) -> HashMap<&'static str, u64> {
    // The gRPC port is configured; the REST API is one below it, by Qdrant's convention.
    let rest = cfg.qdrant.url.rsplit_once(':').map(|(host, port)| format!("{host}:{}", port.parse::<u16>().unwrap_or(6334) - 1)).expect("qdrant url has a port");
    let http = reqwest::Client::new();
    let mut out = HashMap::new();
    for suffix in ["blocks", "sections", "entities"] {
        let url = format!("{rest}/collections/{prefix}_{suffix}");
        let v: Value = match http.get(&url).send().await {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(e) => panic!("querying {url}: {e}"),
        };
        out.insert(suffix, v["result"]["points_count"].as_u64().unwrap_or(0));
    }
    out
}
