//! Against a local Qdrant (`devenv up`). Run with `cargo test -p ds-vectors -- --ignored`.
//!
//! Ported from the HelixDB writer's integration suite: these assert the invariants that
//! survived the store change, not the implementation.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use ds_core::{RunId, TenantId};
use ds_graph::{DocGraph, EmbedTarget, GNode, NodeKind, RunInfo, Vectors};
use ds_vectors::{QdrantConfig, QdrantStore, VectorStore};

const DIMS: usize = 4;

fn config() -> QdrantConfig {
    QdrantConfig {
        url: std::env::var("DS__QDRANT__URL").unwrap_or_else(|_| "http://127.0.0.1:6334".into()),
        api_key: None,
        // A per-process prefix so a failed run never poisons the next one.
        collection_prefix: format!("itest_{}", RunId::new().to_string().replace(['-'], "")),
        batch_size: 8,
        idf: true,
    }
}

fn node(kind: NodeKind, key: &str, props: Value) -> GNode {
    GNode { kind, key: key.into(), props }
}

/// One document run. Block and section keys are run-specific, as real hash keys are;
/// entity keys are not, because entities outlive the run.
fn graph(doc: &str, run: &str) -> DocGraph {
    let (b1, sec, ent) = (format!("{doc}-{run}-b1"), format!("sec:{doc}-{run}-b1"), format!("{doc}-e-bert"));
    DocGraph {
        nodes: vec![
            node(NodeKind::Document, doc, json!({ "title": "A paper" })),
            node(NodeKind::Block, &b1, json!({ "kind": "paragraph", "page_no": 1 })),
            node(NodeKind::Section, &sec, json!({ "title": "1 Introduction", "level": 1 })),
            node(NodeKind::Entity, &ent, json!({ "name": "BERT", "type_id": "method", "aliases": [] })),
        ],
        edges: vec![],
        embed_targets: vec![
            EmbedTarget { node_key: b1, kind: NodeKind::Block, text: "BERT improves GLUE scores".into() },
            EmbedTarget { node_key: sec, kind: NodeKind::Section, text: "1 Introduction".into() },
            EmbedTarget { node_key: ent, kind: NodeKind::Entity, text: "BERT (method)".into() },
        ],
    }
}

fn vectors(doc: &str, run: &str) -> Vectors {
    Vectors {
        model_ref: "test-embedder@1".into(),
        dimensions: DIMS,
        by_key: HashMap::from([
            (format!("{doc}-{run}-b1"), vec![0.9, 0.1, 0.0, 0.0]),
            (format!("sec:{doc}-{run}-b1"), vec![0.0, 0.9, 0.1, 0.0]),
            (format!("{doc}-e-bert"), vec![0.0, 0.0, 0.9, 0.1]),
        ]),
    }
}

fn run_info(tenant: &str, run: RunId, doc: &str) -> RunInfo {
    RunInfo {
        tenant: TenantId(tenant.into()),
        run_id: run,
        language: "en".into(),
        page_count: 1,
        doc_types: vec!["scientific_paper".into()],
            doc_type: None,
        new_entities: HashSet::from([format!("{doc}-e-bert")]),
    }
}

#[tokio::test]
#[ignore = "needs Qdrant from `devenv up`"]
async fn write_switch_and_retry_runs() {
    let store = QdrantStore::new(&config()).unwrap();
    store.ensure_ready(DIMS).await.unwrap();
    let tenant = TenantId("t1".into());
    let doc = "doc1";
    let entity_query = vec![0.0, 0.0, 1.0, 0.0];

    // ── Run 1 ──
    let r1 = RunId::new();
    let run1 = run_info("t1", r1, doc);
    store.write_run(&run1, &graph(doc, "r1"), &vectors(doc, "r1")).await.unwrap();

    let hits = store.search_entities(&tenant, &entity_query, 10).await.unwrap();
    assert_eq!(hits.len(), 1, "the run's entity must be searchable");
    assert_eq!(hits[0].entity_hk, format!("{doc}-e-bert"));
    assert_eq!(hits[0].type_id, "method");
    assert_eq!(
        hits[0].vector.as_deref().map(|v| v.len()),
        Some(DIMS),
        "the stored vector must come back, so the caller can score with its own cosine"
    );

    // ── Run 2, written twice: the second write is the retry a redelivered task makes ──
    let r2 = RunId::new();
    let run2 = run_info("t2_same_tenant", r2, doc);
    let run2 = RunInfo { tenant: tenant.clone(), ..run2 };
    store.write_run(&run2, &graph(doc, "r2"), &vectors(doc, "r2")).await.unwrap();
    store.write_run(&run2, &graph(doc, "r2"), &vectors(doc, "r2")).await.unwrap();

    // Run 1's block and section points are gone; one entity remains, not two.
    let hits = store.search_entities(&tenant, &entity_query, 10).await.unwrap();
    assert_eq!(hits.len(), 1, "writing a run twice must not duplicate the entity");

    // ── Dropping the current run leaves the entity alone ──
    store.drop_run(&tenant, doc, &r2.to_string()).await.unwrap();
    let hits = store.search_entities(&tenant, &entity_query, 10).await.unwrap();
    assert_eq!(hits.len(), 1, "entity vectors outlive the run that created them");

    // ── Tenant isolation ──
    let other = TenantId("someone-else".into());
    let hits = store.search_entities(&other, &entity_query, 10).await.unwrap();
    assert!(hits.is_empty(), "another tenant must not see these entities");
}

#[tokio::test]
#[ignore = "needs Qdrant from `devenv up`"]
async fn entities_of_two_tenants_stay_separate() {
    let store = QdrantStore::new(&config()).unwrap();
    store.ensure_ready(DIMS).await.unwrap();
    let query = vec![0.0, 0.0, 1.0, 0.0];

    for (tenant, doc) in [("acme", "acme-doc"), ("globex", "globex-doc")] {
        let run = run_info(tenant, RunId::new(), doc);
        store.write_run(&run, &graph(doc, "r1"), &vectors(doc, "r1")).await.unwrap();
    }

    for (tenant, doc) in [("acme", "acme-doc"), ("globex", "globex-doc")] {
        let hits = store.search_entities(&TenantId(tenant.into()), &query, 10).await.unwrap();
        assert_eq!(hits.len(), 1, "{tenant} should see exactly its own entity");
        assert_eq!(hits[0].entity_hk, format!("{doc}-e-bert"));
    }
}

/// A zero or mismatched dimension must fail loudly at startup, not silently later.
#[tokio::test]
#[ignore = "needs Qdrant from `devenv up`"]
async fn zero_dimensions_is_a_config_error() {
    let store = QdrantStore::new(&config()).unwrap();
    assert!(store.ensure_ready(0).await.is_err());
}

// ───────────── hybrid search ─────────────
//
// The sparse half was written from the first version of this crate and queried by
// nothing: stored, and therefore unproven. These tests are the half that was missing.

/// A document whose blocks separate the two halves of retrieval: one block a dense
/// embedder would find and a lexical index would not, one the other way round.
fn mixed_graph(doc: &str, run: &str) -> DocGraph {
    let near = format!("{doc}-{run}-near");
    let exact = format!("{doc}-{run}-exact");
    DocGraph {
        nodes: vec![
            node(NodeKind::Document, doc, json!({ "title": "A mixed document" })),
            node(NodeKind::Block, &near, json!({ "kind": "paragraph", "page_no": 1 })),
            node(NodeKind::Block, &exact, json!({ "kind": "paragraph", "page_no": 2 })),
        ],
        edges: vec![],
        embed_targets: vec![
            // Semantically close to the query vector below, with none of its words.
            EmbedTarget { node_key: near, kind: NodeKind::Block, text: "transformer models improve benchmark scores".into() },
            // The identifier a dense embedder blurs away and a lexical index finds
            // exactly — the case `sparse.rs` exists for.
            EmbedTarget { node_key: exact, kind: NodeKind::Block, text: "Invoice INV-2026-0042 total 1499.95 EUR".into() },
        ],
    }
}

fn mixed_vectors(doc: &str, run: &str) -> Vectors {
    Vectors {
        model_ref: "test-embedder@1".into(),
        dimensions: DIMS,
        by_key: HashMap::from([
            (format!("{doc}-{run}-near"), vec![0.95, 0.05, 0.0, 0.0]),
            // Deliberately far from the dense query, so a hit on it can only be lexical.
            (format!("{doc}-{run}-exact"), vec![0.0, 0.0, 0.0, 1.0]),
        ]),
    }
}

#[tokio::test]
#[ignore = "needs Qdrant from `devenv up`"]
async fn dense_lexical_and_fused_search_each_find_what_only_they_can() {
    use ds_vectors::{HybridQuery, Scope};

    let store = QdrantStore::new(&config()).unwrap();
    store.ensure_ready(DIMS).await.unwrap();
    let tenant = TenantId("t1".into());
    let (doc, run) = ("mixed-doc", "r1");
    let info = run_info(tenant.as_str(), RunId::new(), doc);
    store.write_run(&info, &mixed_graph(doc, run), &mixed_vectors(doc, run)).await.unwrap();

    let near = format!("{doc}-{run}-near");
    let exact = format!("{doc}-{run}-exact");
    let dense_query = vec![1.0, 0.0, 0.0, 0.0];

    // Dense alone: finds the paraphrase, ranks the invoice last or not at all.
    let hits = store.search(&tenant, Scope::Blocks, &HybridQuery::dense(dense_query.clone(), 5)).await.unwrap();
    assert_eq!(hits.first().map(|h| h.key.as_str()), Some(near.as_str()), "{hits:?}");

    // Lexical alone: finds the invoice number, which the dense vector is nowhere near.
    // This is the assertion that was impossible before `search` existed — the sparse
    // vectors were written and nothing could ask them anything.
    let hits = store.search(&tenant, Scope::Blocks, &HybridQuery::text("INV-2026-0042", 5)).await.unwrap();
    assert_eq!(hits.first().map(|h| h.key.as_str()), Some(exact.as_str()), "{hits:?}");

    // Fused: both are in the result, which is the point of fusing — neither half alone
    // returns both, and a client given two top-k lists cannot recover this ranking.
    let hits = store
        .search(&tenant, Scope::Blocks, &HybridQuery::hybrid(dense_query.clone(), "INV-2026-0042 benchmark", 5))
        .await
        .unwrap();
    let keys: Vec<&str> = hits.iter().map(|h| h.key.as_str()).collect();
    assert!(keys.contains(&near.as_str()) && keys.contains(&exact.as_str()), "fusion dropped one half: {keys:?}");

    // The payload a reader needs to join a hit back to Postgres.
    let hit = hits.iter().find(|h| h.key == exact).unwrap();
    assert_eq!(hit.document_hk.as_deref(), Some(doc));
    assert_eq!(hit.page_no, Some(2));
    assert!(hit.vector.is_none(), "a plain search must not pay for the stored vector");

    // Scoped to one document, and to one tenant.
    let within = store
        .search(&tenant, Scope::Blocks, &HybridQuery::dense(dense_query.clone(), 5).within("another-doc"))
        .await
        .unwrap();
    assert!(within.is_empty(), "a document filter that matches nothing must return nothing: {within:?}");
    let elsewhere = TenantId("someone-else".into());
    assert!(store.search(&elsewhere, Scope::Blocks, &HybridQuery::text("INV-2026-0042", 5)).await.unwrap().is_empty());

    // A query whose text tokenises to nothing is not an error, and neither is an empty
    // one: both mean "no lexical half", which `search` answers with no hits rather than
    // with a server error.
    assert!(store.search(&tenant, Scope::Blocks, &HybridQuery::text("(3)", 5)).await.unwrap().is_empty());
    assert!(store.search(&tenant, Scope::Blocks, &HybridQuery::dense(vec![], 5)).await.unwrap().is_empty());
    assert!(store.search(&tenant, Scope::Blocks, &HybridQuery::dense(dense_query, 0)).await.unwrap().is_empty());

    // `with_vector` is what corpus resolution needs: it scores with its own cosine rather
    // than trusting the store's metric.
    let entities = store
        .search(&tenant, Scope::Entities, &HybridQuery::dense(vec![0.0, 0.0, 0.9, 0.1], 5).with_vector())
        .await
        .unwrap();
    assert!(entities.is_empty() || entities[0].vector.is_some());
}

/// A dimension that does not match the live collection must be refused before any point
/// is written, with a message naming the way out.
///
/// Before the guard, `ensure_ready` left the collection alone and every *write* failed
/// per point: a mismatch surfaced as every document failing, one at a time, with nothing
/// saying why.
#[tokio::test]
#[ignore = "needs Qdrant from `devenv up`"]
async fn a_dimension_change_is_refused_and_then_migrated() {
    let cfg = config();
    let store = QdrantStore::new(&cfg).unwrap();
    store.ensure_ready(DIMS).await.unwrap();
    // Idempotent at the same dimension.
    store.ensure_ready(DIMS).await.unwrap();

    let err = store.ensure_ready(DIMS * 2).await.expect_err("a different dimension must not be accepted silently");
    let msg = err.to_string();
    assert!(msg.contains("vectors") && msg.contains("vectors migrate"), "the message must say what to do: {msg}");

    // The migration creates the collection for the new dimension and points the
    // configured name at it, so readers follow without being reconfigured.
    let report = store.migrate_dimensions(DIMS * 2).await.expect("migrate");
    assert!(report.iter().any(|line| line.contains(&format!("__d{}", DIMS * 2))), "{report:?}");
    assert!(report.iter().any(|line| line.contains("re-run the documents")), "the report must say what is not migrated: {report:?}");
    // And afterwards the new dimension is the one that is ready.
    store.ensure_ready(DIMS * 2).await.expect("the new dimension is live");
}
