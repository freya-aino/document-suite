//! Against a local Qdrant (`devenv up`). Run with `cargo test -p vs-vectors -- --ignored`.
//!
//! Ported from the HelixDB writer's integration suite: these assert the invariants that
//! survived the store change, not the implementation.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use vs_core::{RunId, TenantId};
use vs_graph::{DocGraph, EmbedTarget, GNode, NodeKind, RunInfo, Vectors};
use vs_vectors::{QdrantConfig, QdrantStore, VectorStore};

const DIMS: usize = 4;

fn config() -> QdrantConfig {
    QdrantConfig {
        url: std::env::var("VS__QDRANT__URL").unwrap_or_else(|_| "http://127.0.0.1:6334".into()),
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
