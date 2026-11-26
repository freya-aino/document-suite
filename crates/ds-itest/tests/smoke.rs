//! End-to-end functionality, over HTTP, against the deployed image.
//!
//! What this asserts and what it does not: on the `fake` tier the model answers are ours,
//! so every number here is about **wiring**, not about parsing. A green run means a
//! document was accepted with a verified token, routed, parsed, stitched, extracted,
//! written to Postgres and Qdrant and read back — not that any of it was done well.
//!
//! `#[ignore]` as the house convention, and `stack_or_skip` on top, so
//! `cargo test --workspace -- --include-ignored` does not fail for someone with no
//! stack running. A test that *is* reached fails loudly.

use std::time::Duration;

use ds_itest::client::{Client, Upload};
use ds_itest::corpus::{freshened, nonce, Corpus};
use ds_itest::stack::stack_or_skip;

const CLIENT_ID: &str = "ds-api";
const SECRET: &str = "ds-api-dev-secret";
const LIST_VERSION: &str = "example-2026-11-06";

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the compose stack: deploy/compose/stack.sh up"]
async fn a_generated_corpus_goes_through_the_deployed_api() -> anyhow::Result<()> {
    let Some(stack) = stack_or_skip().await else { return Ok(()) };
    let client = Client::login(&stack, CLIENT_ID, SECRET).await?;

    // ── the token the realm minted is the one the service is configured for ──
    //
    // Keycloak stamps `iss` from the request it answered, and this token was asked for
    // over the published port while the service inside the network is configured for the
    // in-network URL. Without the realm's frontend URL pinned, every call below would be
    // a wrong-issuer refusal that looks like a key problem.
    let issuer = client.token_issuer()?;
    assert!(issuer.starts_with("http://keycloak:8080/realms/"), "the realm stamped {issuer:?}, which the service will refuse");
    assert_eq!(client.status_without_token("/v1/documents").await?, 401, "the API must refuse an unauthenticated read");

    // ── a DAG list, through the API, validated by the production registry ──
    let list = std::fs::read_to_string(repo_root().join("config/dag-list.example.json"))?;
    client.activate_dag_list(&list, LIST_VERSION).await?;

    let dir = std::env::temp_dir().join(format!("ds-itest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let corpus = Corpus::generate(&dir, 1, 1)?;
    assert_eq!(corpus.entries.len(), 4, "one document of each page shape");

    // Bytes of this run's own, or two tests collide on one document: the generator is
    // deterministic, so "my corpus" and "your corpus" are the same documents.
    let run = nonce("smoke");
    let before = client.metrics(&stack).await?;
    let mut with_rule = 0usize;
    let mut without_hint = 0usize;

    for (i, entry) in corpus.entries.iter().enumerate() {
        // Most uploads carry the caller's document-type hint, as a real one does and as
        // the manifest records. Exactly one deliberately does not: without it a
        // born-digital text-only page skips the vision classifier, its type is
        // undetermined, and it takes the low-confidence DAG — so omitting the hint is the
        // only way the fallback path is exercised at all. Both paths are asserted below.
        let send_hint = i > 0;
        let opts = Upload {
            language: entry.language.clone(),
            profile: "balanced".into(),
            doc_type: send_hint.then(|| entry.doc_type_hint.clone()).flatten(),
        };
        if !send_hint {
            without_hint += 1;
        }

        let bytes = freshened(&std::fs::read(&entry.uri)?, &format!("{run}-{i}"));
        let accepted = client.upload(bytes, &format!("{}.pdf", entry.id()), &opts).await?;
        let job = client.await_job(&accepted.job_id, Duration::from_secs(120)).await?;

        assert_eq!(job.state, "done", "{}: {:?}", entry.id(), job.error);
        assert_eq!(job.pages_failed, 0, "{}: a page failed", entry.id());
        assert_eq!(Some(job.pages_done), job.pages_total, "{}: not every page finished", entry.id());

        // The stage ladder is the only per-document latency source there is, so a gap in
        // it is worth catching here rather than discovering in the latency pass.
        for (from, to) in [("received", "ingesting"), ("ingesting", "pages_processing"), ("pages_processing", "done")] {
            let d = job.between(from, to);
            assert!(d.is_some_and(|d| d >= 0), "{}: no timing from {from} to {to}", entry.id());
        }

        let hk = accepted.document_hk.or(job.document_hk).ok_or_else(|| anyhow::anyhow!("no document hash key"))?;
        let doc = client.document(&hk).await?;
        assert_eq!(doc.run_state.as_deref(), Some("done"), "{}", entry.id());
        assert_eq!(doc.dag_list_version.as_deref(), Some(LIST_VERSION), "{}: the active list was not used", entry.id());
        assert!(!doc.blocks_by_kind.is_empty(), "{}: nothing was stored", entry.id());
        assert!(doc.entities > 0 && doc.assertions > 0, "{}: Stage 2 produced nothing", entry.id());

        for p in &doc.pages {
            // Routing is assertable over HTTP: the DAG that ran and the rule that chose it
            // are both recorded against the page.
            if p.rule_id.is_some() {
                with_rule += 1;
            }
            let page = client.page(&hk, p.page_no as u32).await?;
            assert!(!page.blocks.is_empty(), "{} page {}: no blocks", entry.id(), p.page_no);
            assert_eq!(page.dag_ref, p.dag_ref, "the summary and the page result disagree about the DAG");
            // Every block must be traceable to the step and model that made it: that is
            // the provenance the whole system exists to keep.
            for b in &page.blocks {
                assert!(!b.provenance.step_kind.is_empty(), "a block with no step");
                assert_eq!(b.provenance.dag_ref, page.dag_ref);
            }

            // And every *node* must be traceable, which is the stronger claim and the one
            // schema version 2 exists for. A block's provenance cannot account for a node
            // that produced no block — an optional node that failed, a page conditioner,
            // a verify pass that only re-scored — so without these the host/model split
            // and the parse paths were available in process and not over the API.
            assert_eq!(page.schema_version, 2, "{} page {}: stored at the wrong schema version", entry.id(), p.page_no);
            assert!(!page.traces.is_empty(), "{} page {}: no node traces", entry.id(), p.page_no);
            let levels: Vec<u32> = page.traces.iter().map(|t| t.level).collect();
            assert_eq!(
                *levels.iter().max().expect("a level") as usize + 1,
                levels.iter().collect::<std::collections::BTreeSet<_>>().len(),
                "{} page {}: the executor's levels must be contiguous from 0, got {levels:?}",
                entry.id(),
                p.page_no
            );
            assert!(page.traces.iter().all(|t| !t.step_kind.is_empty() && t.outcome == "ok"));
            // The split itself, and the invariant that makes it trustworthy: neither side
            // may exceed the page, because a level costs its slowest node and not the sum.
            let (host_ms, model_ms) = page.critical_path_ms().expect("traces, so a critical path");
            assert!(
                page.traces.iter().any(|t| t.model_ref.is_some()),
                "{} page {}: no node attributed to the model tier",
                entry.id(),
                p.page_no
            );
            let total: u64 = page.traces.iter().map(|t| t.duration_ms).sum();
            assert!(host_ms + model_ms <= total, "the critical path {host_ms}+{model_ms} exceeds the total node time {total}");
        }
    }

    // ── what the deployment recorded about the work ──
    let after = client.metrics(&stack).await?;
    let d = after.delta(&before);

    assert_eq!(d.sum("dag_node_failures_total"), 0.0, "a DAG node failed");
    for stage in ["ingest", "page", "doc"] {
        let ok = d.get("worker_stage_total", &[("stage", stage), ("outcome", "ok")]).unwrap_or(0.0);
        assert!(ok >= 4.0, "stage {stage} completed {ok} times for 4 documents");
    }
    assert!(d.sum("model_tokens_total") > 0.0, "no tokens were counted, so the model tier was never called");
    assert!(d.sum("dag_pages_total") >= 4.0, "fewer pages ran than were uploaded");
    assert!(after.has("worker_stage_duration_seconds_bucket"), "the latency histograms must be scrapeable");

    // Both routing paths, deliberately: rules for the hinted documents, the
    // low-confidence DAG for the one without a hint.
    assert!(with_rule > 0, "no page was routed by a rule, so content-aware routing was never exercised");
    assert!(
        d.sum("router_low_confidence_total") >= without_hint as f64,
        "the low-confidence path was not taken by the document uploaded without a hint"
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// A second organization must not see the first one's documents.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the compose stack: deploy/compose/stack.sh up"]
async fn another_organization_sees_nothing_of_this_one() -> anyhow::Result<()> {
    let Some(stack) = stack_or_skip().await else { return Ok(()) };
    let t1 = Client::login(&stack, CLIENT_ID, SECRET).await?;
    let t2 = Client::login(&stack, "ds-api-t2", "ds-api-t2-dev-secret").await?;

    let list = std::fs::read_to_string(repo_root().join("config/dag-list.example.json"))?;
    t1.activate_dag_list(&list, LIST_VERSION).await?;

    let dir = std::env::temp_dir().join(format!("ds-itest-tenant-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let corpus = Corpus::generate(&dir, 1, 1)?;
    let entry = &corpus.entries[0];
    let opts = Upload { doc_type: entry.doc_type_hint.clone(), ..Upload::default() };
    let bytes = freshened(&std::fs::read(&entry.uri)?, &nonce("tenant"));
    let accepted = t1.upload(bytes, "tenant.pdf", &opts).await?;
    let job = t1.await_job(&accepted.job_id, Duration::from_secs(120)).await?;
    assert_eq!(job.state, "done");
    let hk = accepted.document_hk.expect("a multipart upload returns the document hash key");

    // The uploader can read it; the other organization cannot, and is told nothing about
    // whether it exists.
    assert!(t1.document(&hk).await.is_ok());
    let refused = t2.document(&hk).await;
    assert!(refused.is_err(), "another organization read this document: {refused:?}");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
