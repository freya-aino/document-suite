//! Against the local Helix instance (`helix push dev` in crates/vs-helix/helix).
//! Run with `cargo test -p vs-helix -- --ignored`.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use vs_core::{RunId, TenantId};
use vs_graph::{DocGraph, EdgeKind, GEdge, GNode, NodeKind};
use vs_helix::{GraphStore, HelixStore, RunInfo, Vectors};

fn url() -> String {
    std::env::var("VS__HELIX__URL").unwrap_or_else(|_| "http://127.0.0.1:6969".into())
}

fn n(kind: NodeKind, key: &str, props: Value) -> GNode {
    GNode { kind, key: key.into(), props }
}
fn e(kind: EdgeKind, from: &str, to: &str, props: Value) -> GEdge {
    GEdge { kind, from: from.into(), to: to.into(), props }
}

/// One document run: block and section keys are run-specific, like real block hash keys.
fn graph(doc: &str, run: &str, text: &str) -> DocGraph {
    let (b1, b2, sec) = (format!("{doc}-{run}-b1"), format!("{doc}-{run}-b2"), format!("sec:{doc}-{run}-b1"));
    let (p1, asr) = (format!("{doc}-p1"), format!("asr:{doc}:a1"));
    let bbox = json!({ "x0": 0.1, "y0": 0.1, "x1": 0.9, "y1": 0.2 });
    DocGraph {
        nodes: vec![
            n(NodeKind::Document, doc, json!({ "title": "A paper" })),
            n(NodeKind::Page, &p1, json!({ "page_no": 1 })),
            n(NodeKind::Section, &sec, json!({ "title": "1 Introduction", "level": 1 })),
            n(NodeKind::Block, &b1, json!({ "kind": "heading", "text": "1 Introduction", "page_no": 1, "global_order": 0, "confidence": 0.99, "bbox": bbox, "content": { "type": "text", "text": "" } })),
            n(NodeKind::Block, &b2, json!({ "kind": "paragraph", "text": text, "page_no": 1, "global_order": 1, "confidence": 0.99, "bbox": bbox, "content": { "type": "text", "text": "" } })),
            n(NodeKind::Entity, &format!("{doc}-e-bert"), json!({ "name": "BERT", "type_id": "method", "aliases": [] })),
            n(NodeKind::Entity, &format!("{doc}-e-glue"), json!({ "name": "GLUE", "type_id": "dataset", "aliases": [] })),
            n(NodeKind::Assertion, &asr, json!({ "predicate": "evaluated on", "predicate_id": "uses", "kind": "binary", "qualifiers": {}, "literals": [], "confidence": 0.8, "verified": true })),
        ],
        edges: vec![
            e(EdgeKind::HasPage, doc, &p1, Value::Null),
            e(EdgeKind::OnPage, &b1, &p1, Value::Null),
            e(EdgeKind::OnPage, &b2, &p1, Value::Null),
            e(EdgeKind::HasSection, doc, &sec, Value::Null),
            e(EdgeKind::Contains, &sec, &b2, Value::Null),
            e(EdgeKind::Next, &b1, &b2, Value::Null),
            e(EdgeKind::Mentions, &b2, &format!("{doc}-e-bert"), json!({ "char_start": 0, "char_end": 4, "surface": "BERT", "confidence": 0.9 })),
            e(EdgeKind::Arg, &asr, &format!("{doc}-e-bert"), json!({ "role": "subject" })),
            e(EdgeKind::Arg, &asr, &format!("{doc}-e-glue"), json!({ "role": "object" })),
            e(EdgeKind::Evidence, &asr, &b2, json!({ "char_start": 0, "char_end": 10 })),
            e(EdgeKind::Related, &format!("{doc}-e-bert"), &format!("{doc}-e-glue"), json!({ "predicate": "uses", "assertion": asr, "confidence": 0.8 })),
        ],
        embed_targets: vec![],
    }
}

fn vectors(doc: &str, run: &str) -> Vectors {
    Vectors {
        model_ref: "test-embedder@1".into(),
        dimensions: 4,
        by_key: HashMap::from([
            (format!("{doc}-{run}-b2"), vec![0.9, 0.1, 0.0, 0.0]),
            (format!("sec:{doc}-{run}-b1"), vec![0.0, 0.9, 0.1, 0.0]),
            (format!("{doc}-e-bert"), vec![0.0, 0.0, 0.9, 0.1]),
            (format!("{doc}-e-glue"), vec![0.1, 0.0, 0.0, 0.9]),
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
        new_entities: HashSet::from([format!("{doc}-e-bert"), format!("{doc}-e-glue")]),
    }
}

fn arr(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        other => vec![other.clone()],
    }
}

#[tokio::test]
#[ignore = "needs a local Helix instance (`helix push dev`)"]
async fn write_switch_and_retry_runs() {
    let store = HelixStore::new(url(), 100).unwrap();
    let tenant = format!("t-{}", uuid::Uuid::now_v7().simple());
    let doc = format!("doc-{}", uuid::Uuid::now_v7().simple());
    let (r1, r2) = (RunId::new(), RunId::new());

    store.write_run(&run_info(&tenant, r1, &doc), &graph(&doc, "r1", "attention is all you need"), &vectors(&doc, "r1")).await.unwrap();
    let outline = store.query("DocumentOutline", &json!({ "tenant": tenant, "document_hk": doc })).await.unwrap();
    let d = &arr(&outline["doc"])[0];
    assert_eq!(d["current_run_id"], r1.to_string(), "{outline}");
    assert_eq!(arr(&outline["sections"]).len(), 1);
    assert_eq!(arr(&outline["pages"]).len(), 1);

    let hits = store.query("SearchBlocksText", &json!({ "tenant": tenant, "text": "attention", "k": 10 })).await.unwrap();
    assert!(arr(&hits["blocks"]).iter().any(|b| b["text"] == "attention is all you need"), "{hits}");
    let near = store.query("SearchBlocks", &json!({ "tenant": tenant, "vector": [0.9, 0.1, 0.0, 0.0], "k": 5 })).await.unwrap();
    assert!(arr(&near["blocks"]).iter().any(|b| b["block_hk"] == format!("{doc}-r1-b2")), "{near}");
    let facts = store.query("EntityAssertions", &json!({ "tenant": tenant, "entity_hk": format!("{doc}-e-bert") })).await.unwrap();
    assert_eq!(arr(&facts["assertions"]).len(), 1, "{facts}");
    assert_eq!(arr(&facts["evidence"]).len(), 1);

    // Reprocessing: the new run becomes current and the old run disappears.
    store.write_run(&run_info(&tenant, r2, &doc), &graph(&doc, "r2", "a second parse"), &vectors(&doc, "r2")).await.unwrap();
    // A retry of the same run must not duplicate anything.
    store.write_run(&run_info(&tenant, r2, &doc), &graph(&doc, "r2", "a second parse"), &vectors(&doc, "r2")).await.unwrap();

    let outline = store.query("DocumentOutline", &json!({ "tenant": tenant, "document_hk": doc })).await.unwrap();
    assert_eq!(arr(&outline["doc"])[0]["current_run_id"], r2.to_string());
    assert_eq!(arr(&outline["sections"]).len(), 1, "{outline}");
    assert_eq!(arr(&outline["pages"]).len(), 1, "{outline}");
    let old = store.query("SearchBlocksText", &json!({ "tenant": tenant, "text": "attention", "k": 10 })).await.unwrap();
    assert!(arr(&old["blocks"]).is_empty(), "old run still searchable: {old}");
    let facts = store.query("EntityAssertions", &json!({ "tenant": tenant, "entity_hk": format!("{doc}-e-bert") })).await.unwrap();
    assert_eq!(arr(&facts["assertions"]).len(), 1, "{facts}");
    let related = store.query("RelatedEntities", &json!({ "tenant": tenant, "entity_hk": format!("{doc}-e-bert") })).await.unwrap();
    assert_eq!(arr(&related["edges"]).len(), 1, "{related}");

    // Entities are shared: the unique constraint rejects a second create, and the
    // entity kept one name vector.
    let dup = store
        .query("CreateEntityWithoutVector", &json!({ "tenant": tenant, "uk": format!("entity:{doc}-e-bert"), "entity_hk": format!("{doc}-e-bert"), "name": "x", "type_id": "x", "aliases": [] }))
        .await
        .unwrap_err();
    assert!(dup.is_duplicate(), "{dup}");
    let names = store.query("SearchEntityNames", &json!({ "tenant": tenant, "vector": [0.0, 0.0, 0.9, 0.1], "k": 10 })).await.unwrap();
    let bert: Vec<_> = arr(&names["entities"]).into_iter().filter(|e| e["entity_hk"] == format!("{doc}-e-bert")).collect();
    assert_eq!(bert.len(), 1, "{names}");

    // Tenant isolation: another tenant's lookup finds nothing (Helix 2.3.5 reports a
    // single-valued lookup that finds nothing as "No value found").
    let other = store.query("DocumentOutline", &json!({ "tenant": "someone-else", "document_hk": doc })).await.unwrap_err();
    assert!(other.is_not_found(), "{other}");
}
