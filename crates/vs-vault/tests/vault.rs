//! Against Postgres from `devenv up` (`DATABASE_URL`; the role needs CREATEDB, granted by
//! devenv). Each test gets a fresh migrated database. Run with
//! `cargo test -p vs-vault -- --ignored`.

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use vs_core::*;
use vs_vault::ops::{Ops, PageOutcome};
use vs_vault::{DocumentMeta, PgVault, RunMeta, VaultWriter};

fn spec() -> JobSpec {
    JobSpec {
        tenant: TenantId("t1".into()),
        language: LanguageTag::parse("en").unwrap(),
        priority: Priority::Batch,
        profile: Profile::Balanced,
        dag_list_version: None,
        source_uri: "originals/t1/abc".into(),
        external_ref: Some("ext-1".into()),
    }
}

fn meta(pages: u32) -> DocumentMeta {
    let sha = content_sha256(b"doc");
    DocumentMeta {
        document_hk: HashKey::document(&TenantId("t1".into()), &sha),
        content_sha256: sha,
        filename: Some("a.pdf".into()),
        size_bytes: 3,
        format: "pdf".into(),
        page_count: pages,
    }
}

fn run() -> RunMeta {
    RunMeta { run_id: RunId::new(), dag_list_version: "dl-1".into(), started_at: Utc::now() }
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn write_document_is_idempotent_and_versions_runs(pool: PgPool) {
    let v = PgVault { pool: pool.clone() };
    let (r1, m) = (run(), meta(3));
    v.write_document(&r1, &spec(), &m).await.unwrap();
    v.write_document(&r1, &spec(), &m).await.unwrap(); // retry

    assert_eq!(count(&pool, "vault.hub_document").await, 1);
    assert_eq!(count(&pool, "vault.sat_document_meta").await, 1);
    assert_eq!(count(&pool, "vault.hub_run").await, 1);
    assert_eq!(count(&pool, "vault.sat_run").await, 1);
    assert_eq!(count(&pool, "vault.lnk_run_document").await, 1);
    assert_eq!(count(&pool, "vault.hub_page").await, 3);
    assert_eq!(count(&pool, "vault.lnk_document_page").await, 3);

    // Reprocessing: same document, new run. Unchanged metadata adds no satellite row.
    let r2 = run();
    v.write_document(&r2, &spec(), &m).await.unwrap();
    assert_eq!(count(&pool, "vault.hub_run").await, 2);
    assert_eq!(count(&pool, "vault.sat_document_meta").await, 1);
    assert_eq!(count(&pool, "vault.hub_page").await, 3);
    let current: Uuid = sqlx::query_scalar("SELECT run_id FROM vault.v_current_run WHERE document_hk = $1")
        .bind(m.document_hk.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(current, r2.run_id.0);

    // Changed metadata adds a satellite row.
    let mut s = spec();
    s.external_ref = Some("ext-2".into());
    v.write_document(&r2, &s, &m).await.unwrap();
    assert_eq!(count(&pool, "vault.sat_document_meta").await, 2);
}

fn block(page_hk: &HashKey, run: RunId, i: u32, model: Option<&str>) -> Block {
    Block {
        block_hk: HashKey::block(page_hk, &run, i),
        index: i,
        kind: BlockKind::Paragraph,
        bbox: BBox::new(0.1, 0.1 * i as f32, 0.9, 0.1 * i as f32 + 0.05).unwrap(),
        reading_order: i,
        content: BlockContent::Text { text: format!("block {i}") },
        text: format!("block {i}"),
        level: None,
        confidence: 0.9,
        provenance: Provenance {
            run_id: run,
            dag_ref: "born-digital@3".into(),
            node_id: if model.is_some() { "parse".into() } else { "merge".into() },
            step_kind: "x".into(),
            model_ref: model.map(String::from),
            prompt_ref: None,
            params_hash: "p".into(),
            started_at: Utc::now(),
            duration_ms: 5,
        },
    }
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn write_page_stores_blocks_with_lineage(pool: PgPool) {
    let v = PgVault { pool: pool.clone() };
    let (r, m) = (run(), meta(1));
    v.write_document(&r, &spec(), &m).await.unwrap();

    let page_hk = HashKey::page(&m.document_hk, 1);
    let page = PageResult {
        schema_version: PageResult::SCHEMA_VERSION,
        document_hk: m.document_hk.clone(),
        page_hk: page_hk.clone(),
        page_no: 1,
        run_id: r.run_id,
        render_width_px: 1653,
        render_height_px: 2339,
        profile: PageProfile {
            language: LanguageTag::parse("en").unwrap(),
            doc_type: DocType::ScientificPaper,
            born_digital: true,
            text_layer_coverage: 1.0,
            has_handwriting: false,
            has_images: false,
            has_tables: false,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 1.0,
            tier: ClassifierTier::Rules,
        },
        dag_ref: "born-digital@3".into(),
        blocks: vec![block(&page_hk, r.run_id, 0, Some("org@layout@2026-01")), block(&page_hk, r.run_id, 1, None)],
    };
    v.write_page(&page, Some("rule-1")).await.unwrap();
    v.write_page(&page, Some("rule-1")).await.unwrap(); // retry

    assert_eq!(count(&pool, "vault.sat_page_profile").await, 1);
    assert_eq!(count(&pool, "vault.hub_block").await, 2);
    assert_eq!(count(&pool, "vault.lnk_page_block").await, 2);
    assert_eq!(count(&pool, "vault.sat_block_content").await, 2);
    assert_eq!(count(&pool, "vault.sat_block_provenance").await, 2);
    assert_eq!(count(&pool, "vault.hub_dag").await, 1);
    assert_eq!(count(&pool, "vault.hub_model").await, 1);
    assert_eq!(count(&pool, "vault.lnk_block_dag_model").await, 2);

    let (model_id, revision): (String, String) = sqlx::query_as("SELECT model_id, revision FROM vault.hub_model")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((model_id.as_str(), revision.as_str()), ("org@layout", "2026-01"));
    let cpu_blocks: i64 = sqlx::query_scalar("SELECT count(*) FROM vault.lnk_block_dag_model WHERE model_hk IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cpu_blocks, 1);
    let text: String = sqlx::query_scalar("SELECT text FROM vault.sat_block_content c JOIN vault.hub_block b USING (block_hk) WHERE b.block_index = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(text, "block 1");
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn page_counters_complete_job_exactly_once(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let (job, r, s) = (Uuid::now_v7(), RunId::new(), spec());
    ops.create_job(job, None, r, &s).await.unwrap();
    let row = ops.get_job(&s.tenant, job).await.unwrap().unwrap();
    assert_eq!(row.state, "received");
    assert!(ops.get_job(&TenantId("other".into()), job).await.unwrap().is_none());

    let doc = meta(3).document_hk;
    ops.start_pages(job, &doc, 3).await.unwrap();
    ops.start_pages(job, &doc, 3).await.unwrap(); // retry

    ops.page_started(job, 1).await.unwrap();
    let p = ops.page_finished(job, 1, PageOutcome::Done, Some("d@1"), None, None).await.unwrap();
    assert!(p.counted && !p.completed_job());
    let again = ops.page_finished(job, 1, PageOutcome::Done, Some("d@1"), None, None).await.unwrap();
    assert!(!again.counted && again.done == 1);

    ops.page_finished(job, 2, PageOutcome::Failed, None, None, Some("boom")).await.unwrap();
    let last = ops.page_finished(job, 3, PageOutcome::Done, Some("d@1"), None, None).await.unwrap();
    assert!(last.completed_job());
    assert_eq!((last.done, last.failed, last.total), (2, 1, 3));
    assert!(!ops.page_finished(job, 3, PageOutcome::Done, None, None, None).await.unwrap().completed_job());

    let row = ops.get_job(&s.tenant, job).await.unwrap().unwrap();
    assert_eq!((row.state.as_str(), row.pages_total, row.document_hk.as_deref()), ("pages_processing", Some(3), Some(doc.as_str())));
}
