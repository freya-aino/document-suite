//! The vault's graph layer and `PgCorpusResolver` against Postgres from `devenv up`.
//!
//! Everything added with the storage rewrite had unit tests of its *column projection*
//! only — "do we build the right arrays" — so no statement in migrations 0005 to 0007 had
//! ever been parsed by a Postgres and no row had ever been inserted by these paths. These
//! tests execute them.
//!
//! Run with `cargo test -p vs-vault --test graph -- --ignored`.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::PgPool;

use vs_core::{
    content_sha256, DocType, DocTypeSource, DocumentType, HashKey, JobSpec, LanguageTag, Priority, Profile, RunId,
    TenantId,
};
use vs_extract::ExtractionOutput;
use vs_graph::{DocGraph, EdgeKind, GEdge, GNode, NodeKind, RunInfo, Vectors};
use vs_models::{Embedder, ModelError};
use vs_stitch::StitchedDocument;
use vs_vault::{DocumentMeta, PgCorpusResolver, PgVault, RunMeta, VaultWriter};
use vs_vectors::{EntityHit, VectorError, VectorStore};

const TENANT: &str = "t1";

fn tenant() -> TenantId {
    TenantId(TENANT.into())
}

fn spec() -> JobSpec {
    JobSpec {
        tenant: tenant(),
        language: LanguageTag::parse("en").unwrap(),
        priority: Priority::Batch,
        profile: Profile::Balanced,
        dag_list_version: None,
        source_uri: "originals/t1/abc".into(),
        external_ref: None,
        doc_type: None,
        owner: None,
    }
}

fn meta() -> DocumentMeta {
    let sha = content_sha256(b"graph-doc");
    DocumentMeta {
        document_hk: HashKey::document(&tenant(), &sha),
        content_sha256: sha,
        filename: Some("a.pdf".into()),
        size_bytes: 9,
        format: "pdf".into(),
        page_count: 1,
    }
}

fn run_meta() -> RunMeta {
    RunMeta { run_id: RunId::new(), dag_list_version: "dl-1".into(), started_at: Utc::now() }
}

/// Returns one fixed hit, so the resolver's Postgres lookup is what is under test.
struct OneHit {
    entity_hk: String,
    type_id: String,
    vector: Option<Vec<f32>>,
}

#[async_trait]
impl VectorStore for OneHit {
    async fn ensure_ready(&self, _: usize) -> Result<(), VectorError> {
        Ok(())
    }
    async fn write_run(&self, _: &RunInfo, _: &DocGraph, _: &Vectors) -> Result<(), VectorError> {
        Ok(())
    }
    async fn drop_run(&self, _: &TenantId, _: &str, _: &str) -> Result<(), VectorError> {
        Ok(())
    }
    async fn search_entities(&self, _: &TenantId, _: &[f32], _: usize) -> Result<Vec<EntityHit>, VectorError> {
        Ok(vec![EntityHit {
            entity_hk: self.entity_hk.clone(),
            type_id: self.type_id.clone(),
            vector: self.vector.clone(),
        }])
    }
}

struct UnitEmbedder;

#[async_trait]
impl Embedder for UnitEmbedder {
    fn model_ref(&self) -> String {
        "test@1".into()
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        Ok(texts.iter().map(|_| vec![0.0, 0.0, 0.0, 1.0]).collect())
    }
}

fn node(kind: NodeKind, key: &str, props: Value) -> GNode {
    GNode { kind, key: key.into(), props }
}

fn edge(kind: EdgeKind, from: &str, to: &str, props: Value) -> GEdge {
    GEdge { kind, from: from.into(), to: to.into(), props }
}

fn hex(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

struct Fixture {
    graph: DocGraph,
    run: RunInfo,
    document_hk: String,
    heading: String,
    table: String,
    figure: String,
    entity_a: String,
}

/// A graph exercising every table migrations 0005 to 0007 added.
fn fixture(document_hk: &HashKey, run_id: RunId) -> Fixture {
    let dk = document_hk.to_string();
    let (heading, body, table, figure) = (hex('1'), hex('2'), hex('3'), hex('4'));
    let (entity_a, entity_b, reference) = (hex('a'), hex('b'), hex('c'));
    let sec = format!("sec:{heading}");
    let sub_heading = hex('5');
    let sub = format!("sec:{sub_heading}");
    let cell = format!("cell:{table}:1:1");
    let img = format!("imgtext:{figure}:0");

    let graph = DocGraph {
        nodes: vec![
            node(NodeKind::Document, &dk, json!({ "title": "A paper" })),
            node(NodeKind::Section, &sec, json!({ "title": "1 Results", "level": 1, "heading_block": heading })),
            node(NodeKind::Section, &sub, json!({ "title": "1.1 Tables", "level": 2, "heading_block": sub_heading })),
            node(NodeKind::Cell, &cell, json!({ "row": 1, "col": 1, "header": false, "text": "88.5", "column_header": "F1" })),
            node(NodeKind::ImageText, &img, json!({ "text": "BERT", "index": 0 })),
        ],
        edges: vec![
            // Document → top-level section, then nesting.
            edge(EdgeKind::HasSection, &dk, &sec, Value::Null),
            edge(EdgeKind::HasSection, &sec, &sub, Value::Null),
            edge(EdgeKind::Contains, &sec, &body, Value::Null),
            edge(EdgeKind::Contains, &sub, &table, Value::Null),
            edge(
                EdgeKind::Related,
                &entity_a,
                &entity_b,
                json!({ "predicate": "evaluated_on", "assertion": "asr:doc:1", "confidence": 0.75 }),
            ),
            edge(EdgeKind::Cites, &body, &reference, json!({ "assertion": "asr:doc:2", "confidence": 0.9 })),
            edge(EdgeKind::HasCell, &table, &cell, Value::Null),
            edge(EdgeKind::TextInImage, &figure, &img, Value::Null),
            // One match of each shape, which is what the CHECK constraint guards.
            edge(EdgeKind::MatchesText, &img, &body, json!({ "via": "block_text" })),
            edge(EdgeKind::MatchesText, &img, &entity_a, json!({ "via": "entity_name" })),
        ],
        embed_targets: vec![],
    };
    let run = RunInfo {
        tenant: tenant(),
        run_id,
        language: "en".into(),
        page_count: 1,
        doc_types: vec!["scientific_paper".into()],
        doc_type: Some(DocumentType {
            doc_type: DocType::ScientificPaper,
            confidence: 0.93,
            source: DocTypeSource::PageVote,
        }),
        new_entities: HashSet::new(),
    };
    Fixture { graph, run, document_hk: dk, heading, table, figure, entity_a }
}

/// Writes the fixture's graph the way production does: through the one transaction that
/// writes the Data Vault rows and the graph layer together.
///
/// The Data Vault half is empty here on purpose. These tests are about migrations 0005
/// to 0007, and an empty `StitchedDocument` is the smallest input that still reaches the
/// graph layer through the real entry point. Nothing in the vault has foreign keys, so
/// graph rows referring to blocks that were never written are inserted as given — which
/// is what lets the fixture use readable synthetic keys.
async fn write_graph(pool: &PgPool, r: &RunMeta, m: &DocumentMeta, f: &Fixture) {
    let doc = StitchedDocument {
        document_hk: m.document_hk.clone(),
        run_id: r.run_id,
        blocks: vec![],
        furniture: Default::default(),
        sections: vec![],
        links: vec![],
        missing_pages: vec![],
    };
    PgVault { pool: pool.clone() }
        .write_stage2(r, &spec(), &doc, &ExtractionOutput::default(), &f.graph, f.run.doc_type.as_ref())
        .await
        .unwrap();
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn write_run_populates_every_graph_table_and_is_idempotent(pool: PgPool) {
    let m = meta();
    let r = run_meta();
    PgVault { pool: pool.clone() }.write_document(&r, &spec(), &m).await.unwrap();

    let f = fixture(&m.document_hk, r.run_id);
    write_graph(&pool, &r, &m, &f).await;
    // A redelivered task writes the identical run again; nothing may double.
    write_graph(&pool, &r, &m, &f).await;

    assert_eq!(count(&pool, "vault.hub_section").await, 2);
    assert_eq!(count(&pool, "vault.sat_section").await, 2);
    assert_eq!(count(&pool, "vault.lnk_section_section").await, 2);
    assert_eq!(count(&pool, "vault.lnk_section_block").await, 2);
    assert_eq!(count(&pool, "vault.lnk_entity_entity").await, 1);
    assert_eq!(count(&pool, "vault.sat_related").await, 1);
    assert_eq!(count(&pool, "vault.lnk_block_entity_cites").await, 1);
    assert_eq!(count(&pool, "vault.sat_document_type").await, 1);
    assert_eq!(count(&pool, "vault.hub_cell").await, 1);
    assert_eq!(count(&pool, "vault.sat_cell").await, 1);
    assert_eq!(count(&pool, "vault.hub_image_text").await, 1);
    assert_eq!(count(&pool, "vault.sat_image_text").await, 1);
    assert_eq!(count(&pool, "vault.lnk_image_text_match").await, 2);
}

/// A top-level section is stored with a NULL parent rather than in its own table, so this
/// is how the document-to-section edge survives.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_section_tree_keeps_its_shape(pool: PgPool) {
    let m = meta();
    let r = run_meta();
    PgVault { pool: pool.clone() }.write_document(&r, &spec(), &m).await.unwrap();
    let f = fixture(&m.document_hk, r.run_id);
    write_graph(&pool, &r, &m, &f).await;

    let top: i64 = sqlx::query_scalar("SELECT count(*) FROM vault.lnk_section_section WHERE parent_section_hk IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(top, 1, "exactly one top-level section");
    let nested: i64 = sqlx::query_scalar("SELECT count(*) FROM vault.lnk_section_section WHERE parent_section_hk IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(nested, 1);

    // The section's own key is derived from the heading block that opens it.
    let expected = HashKey::section(&vs_vault::hash_key_from_hex(&f.heading).unwrap()).to_string();
    let stored: String =
        sqlx::query_scalar("SELECT section_hk FROM vault.hub_section WHERE heading_block_hk = $1")
            .bind(&f.heading)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.trim(), expected);
    assert_eq!(count(&pool, "vault.hub_section").await, 2);
    let doc: String = sqlx::query_scalar("SELECT document_hk FROM vault.hub_section LIMIT 1").fetch_one(&pool).await.unwrap();
    assert_eq!(doc.trim(), f.document_hk);
}

/// `column_header` is the nullable text column bound through `UNNEST`, which is the bind
/// most likely to be wrong and impossible to catch without executing it.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_cell_carries_its_position_and_column_header(pool: PgPool) {
    let m = meta();
    let r = run_meta();
    PgVault { pool: pool.clone() }.write_document(&r, &spec(), &m).await.unwrap();
    let mut f = fixture(&m.document_hk, r.run_id);
    // A second cell with no column header, so both the Some and None binds are exercised.
    let bare = format!("cell:{}:2:0", f.table);
    f.graph.nodes.push(node(NodeKind::Cell, &bare, json!({ "row": 2, "col": 0, "header": false, "text": "RoBERTa" })));
    f.graph.edges.push(edge(EdgeKind::HasCell, &f.table, &bare, Value::Null));

    write_graph(&pool, &r, &m, &f).await;

    let (text, header, column_header): (String, bool, Option<String>) = sqlx::query_as(
        "SELECT s.text, s.header, s.column_header FROM vault.sat_cell s
         JOIN vault.hub_cell h ON h.cell_hk = s.cell_hk
         WHERE h.row_idx = 1 AND h.col_idx = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((text.as_str(), header, column_header.as_deref()), ("88.5", false, Some("F1")));

    let none: Option<String> = sqlx::query_scalar(
        "SELECT s.column_header FROM vault.sat_cell s
         JOIN vault.hub_cell h ON h.cell_hk = s.cell_hk WHERE h.row_idx = 2",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(none, None, "a cell in a table with no header row stores NULL");

    let block: String = sqlx::query_scalar("SELECT block_hk FROM vault.hub_cell WHERE row_idx = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(block.trim(), f.table);
}

/// The CHECK says exactly one of `block_hk` and `entity_hk` is set. `UNNEST`-inserted
/// NULLs are the case that could silently violate it.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn an_image_text_match_targets_exactly_one_thing(pool: PgPool) {
    let m = meta();
    let r = run_meta();
    PgVault { pool: pool.clone() }.write_document(&r, &spec(), &m).await.unwrap();
    let f = fixture(&m.document_hk, r.run_id);
    write_graph(&pool, &r, &m, &f).await;

    let rows: Vec<(Option<String>, Option<String>, String)> =
        sqlx::query_as("SELECT block_hk, entity_hk, via FROM vault.lnk_image_text_match ORDER BY via")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    let (block, entity, via) = &rows[0];
    assert_eq!(via, "block_text");
    assert!(block.is_some() && entity.is_none());
    let (block, entity, via) = &rows[1];
    assert_eq!(via, "entity_name");
    assert!(entity.as_deref().map(str::trim) == Some(f.entity_a.as_str()) && block.is_none());

    let img_block: String = sqlx::query_scalar("SELECT block_hk FROM vault.hub_image_text").fetch_one(&pool).await.unwrap();
    assert_eq!(img_block.trim(), f.figure);
    let text: String = sqlx::query_scalar("SELECT text FROM vault.sat_image_text").fetch_one(&pool).await.unwrap();
    assert_eq!(text, "BERT");
}

/// The document type's satellite is insert-only and versioned by `hash_diff`, so
/// reclassifying adds a row and re-running the same classification does not.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_document_type_versions_only_when_it_changes(pool: PgPool) {
    let m = meta();
    let r = run_meta();
    PgVault { pool: pool.clone() }.write_document(&r, &spec(), &m).await.unwrap();
    let mut f = fixture(&m.document_hk, r.run_id);
    write_graph(&pool, &r, &m, &f).await;
    write_graph(&pool, &r, &m, &f).await;
    assert_eq!(count(&pool, "vault.sat_document_type").await, 1, "the same verdict must not version");

    let (doc_type, confidence, source): (String, f32, String) =
        sqlx::query_as("SELECT doc_type, confidence, source FROM vault.sat_document_type").fetch_one(&pool).await.unwrap();
    assert_eq!((doc_type.as_str(), source.as_str()), ("scientific_paper", "page_vote"));
    assert!((confidence - 0.93).abs() < 1e-5);

    // A caller overriding the type is a different fact about the same document.
    f.run.doc_type = Some(DocumentType { doc_type: DocType::Report, confidence: 1.0, source: DocTypeSource::Caller });
    write_graph(&pool, &r, &m, &f).await;
    assert_eq!(count(&pool, "vault.sat_document_type").await, 2);
    let latest: String =
        sqlx::query_scalar("SELECT doc_type FROM vault.sat_document_type ORDER BY load_dts DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(latest, "report");
}

/// `load_labels`' `JOIN LATERAL` against `sat_entity_label` is the one query standing
/// between a document and a wrong entity link, and it had never been executed.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_resolver_reads_entity_labels_from_postgres(pool: PgPool) {
    // Seed one corpus entity the way `write_stage2` does: a hub row plus its newest
    // label satellite.
    let entity_hk = HashKey::entity(&tenant(), "method|bert").to_string();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO vault.hub_entity (entity_hk, tenant_id, canonical_key, load_dts, record_source)
         VALUES ($1, $2, 'method|bert', $3, 'test')",
    )
    .bind(&entity_hk)
    .bind(TENANT)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    for (name, aliases, when) in [
        ("Stale Name", json!([]), now - chrono::Duration::hours(1)),
        ("BERT", json!(["Bidirectional Encoder"]), now),
    ] {
        sqlx::query(
            "INSERT INTO vault.sat_entity_label (entity_hk, load_dts, hash_diff, record_source, name, type_id, raw_type, aliases, review_status)
             VALUES ($1, $2, $3, 'test', $4, 'method', 'method', $5, 'approved')",
        )
        .bind(&entity_hk)
        .bind(when)
        .bind("0".repeat(64))
        .bind(name)
        .bind(&aliases)
        .execute(&pool)
        .await
        .unwrap();
    }

    let hits = OneHit { entity_hk: entity_hk.clone(), type_id: "method".into(), vector: Some(vec![0.0, 0.0, 0.0, 1.0]) };
    let resolver = PgCorpusResolver::new(pool.clone(), Arc::new(hits), Arc::new(UnitEmbedder));
    let entity = vs_extract::DocEntity {
        local_id: "e1".into(),
        name: "BERT".into(),
        raw_type: "method".into(),
        type_id: Some("method".into()),
        aliases: vec![],
        mentions: vec![],
        entity_hk: None,
        context: String::new(),
    };

    use vs_extract::resolve::{CorpusResolver, Resolution};
    match resolver.resolve(&tenant(), &entity, "BERT reaches 88.5 F1.").await.unwrap() {
        Resolution::Link { entity_hk: linked, score } => {
            assert_eq!(linked.as_str(), entity_hk, "must link to the seeded entity");
            // Identical name and an identical vector: the newest label row was used, not
            // the stale one, or the name half of the score would have dragged it down.
            assert!(score > 0.99, "score was {score}, so the latest label row was not the one read");
        }
        other => panic!("expected a link, got {other:?}"),
    }
}

/// Another tenant's entity must not be offered as a candidate even when the vector store
/// hands it over, because the vault is the system of record and should not trust it.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_resolver_refuses_another_tenants_entity(pool: PgPool) {
    let other = TenantId("someone-else".into());
    let entity_hk = HashKey::entity(&other, "method|bert").to_string();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO vault.hub_entity (entity_hk, tenant_id, canonical_key, load_dts, record_source)
         VALUES ($1, $2, 'method|bert', $3, 'test')",
    )
    .bind(&entity_hk)
    .bind(other.as_str())
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vault.sat_entity_label (entity_hk, load_dts, hash_diff, record_source, name, type_id, raw_type, aliases, review_status)
         VALUES ($1, $2, $3, 'test', 'BERT', 'method', 'method', '[]', 'approved')",
    )
    .bind(&entity_hk)
    .bind(now)
    .bind("0".repeat(64))
    .execute(&pool)
    .await
    .unwrap();

    let hits = OneHit { entity_hk, type_id: "method".into(), vector: Some(vec![0.0, 0.0, 0.0, 1.0]) };
    let resolver = PgCorpusResolver::new(pool.clone(), Arc::new(hits), Arc::new(UnitEmbedder));
    let entity = vs_extract::DocEntity {
        local_id: "e1".into(),
        name: "BERT".into(),
        raw_type: "method".into(),
        type_id: Some("method".into()),
        aliases: vec![],
        mentions: vec![],
        entity_hk: None,
        context: String::new(),
    };

    use vs_extract::resolve::{CorpusResolver, Resolution};
    let got = resolver.resolve(&tenant(), &entity, "").await.unwrap();
    assert!(
        matches!(got, Resolution::New { .. }),
        "a hit from another tenant must not become a link, got {got:?}"
    );
}
