//! Against Postgres from `devenv up` (`DATABASE_URL`; the role needs CREATEDB, granted by
//! devenv). Each test gets a fresh migrated database. Run with
//! `cargo test -p vs-vault -- --ignored`.

use std::collections::HashSet;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use vs_core::*;
use vs_extract::{
    Arg, ArgValue, Assertion, AssertionKind, DocEntity, ExtractionOutput, Mention, Qualifiers,
};
use vs_stitch::{DocBlock, Link, LinkKind, StitchedDocument};
use vs_vault::ops::{NewJob, Ops, PageOutcome};
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
    RunMeta {
        run_id: RunId::new(),
        dag_list_version: "dl-1".into(),
        started_at: Utc::now(),
    }
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
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
    let current: Uuid =
        sqlx::query_scalar("SELECT run_id FROM vault.v_current_run WHERE document_hk = $1")
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
        content: BlockContent::Text {
            text: format!("block {i}"),
        },
        text: format!("block {i}"),
        level: None,
        confidence: 0.9,
        provenance: Provenance {
            run_id: run,
            dag_ref: "born-digital@3".into(),
            node_id: if model.is_some() {
                "parse".into()
            } else {
                "merge".into()
            },
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
        blocks: vec![
            block(&page_hk, r.run_id, 0, Some("org@layout@2026-01")),
            block(&page_hk, r.run_id, 1, None),
        ],
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

    let (model_id, revision): (String, String) =
        sqlx::query_as("SELECT model_id, revision FROM vault.hub_model")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (model_id.as_str(), revision.as_str()),
        ("org@layout", "2026-01")
    );
    let cpu_blocks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM vault.lnk_block_dag_model WHERE model_hk IS NULL")
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
    assert!(ops
        .get_job(&TenantId("other".into()), job)
        .await
        .unwrap()
        .is_none());

    let doc = meta(3).document_hk;
    ops.start_pages(job, &doc, 3).await.unwrap();
    ops.start_pages(job, &doc, 3).await.unwrap(); // retry

    ops.page_started(job, 1).await.unwrap();
    let p = ops
        .page_finished(job, 1, PageOutcome::Done, Some("d@1"), None, None)
        .await
        .unwrap();
    assert!(p.counted && !p.completed_job());
    let again = ops
        .page_finished(job, 1, PageOutcome::Done, Some("d@1"), None, None)
        .await
        .unwrap();
    assert!(!again.counted && again.done == 1);

    ops.page_finished(job, 2, PageOutcome::Failed, None, None, Some("boom"))
        .await
        .unwrap();
    let last = ops
        .page_finished(job, 3, PageOutcome::Done, Some("d@1"), None, None)
        .await
        .unwrap();
    assert!(last.completed_job());
    assert_eq!((last.done, last.failed, last.total), (2, 1, 3));
    // A redelivered last page does not count again, but still reports the job as
    // finished so the page worker can re-publish a lost doc task.
    let redelivered = ops.page_finished(job, 3, PageOutcome::Done, None, None, None).await.unwrap();
    assert!(!redelivered.counted && !redelivered.completed_job() && redelivered.all_finished());
    assert_eq!((redelivered.done, redelivered.failed, redelivered.total), (2, 1, 3));

    let row = ops.get_job(&s.tenant, job).await.unwrap().unwrap();
    assert_eq!(
        (
            row.state.as_str(),
            row.pages_total,
            row.document_hk.as_deref()
        ),
        ("pages_processing", Some(3), Some(doc.as_str()))
    );
}

/// Connects to the same (test) database as the runtime role.
async fn as_app(pool: &PgPool) -> PgPool {
    let db = pool.connect_options().get_database().unwrap().to_string();
    let opts = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(5432)
        .username("vs_app")
        .password(&std::env::var("VS__POSTGRES__PASSWORD").unwrap_or_else(|_| "vs_app".into()))
        .database(&db);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn runtime_role_is_insert_only_on_the_vault(pool: PgPool) {
    let app = as_app(&pool).await;
    vs_vault::check_migrations(&app).await.unwrap();

    // The runtime role can do the pipeline's writes...
    let v = PgVault { pool: app.clone() };
    v.write_document(&run(), &spec(), &meta(2)).await.unwrap();
    let ops = Ops { pool: app.clone() };
    let job = Uuid::now_v7();
    ops.create_job(job, None, RunId::new(), &spec())
        .await
        .unwrap();
    ops.set_state(job, JobState::Failed, Some("x"))
        .await
        .unwrap();

    // ...but cannot rewrite or remove vault history, or change the schema.
    for stmt in [
        "UPDATE vault.hub_document SET tenant_id = 'x'",
        "DELETE FROM vault.sat_document_meta",
        "TRUNCATE vault.hub_page",
        "DROP TABLE vault.hub_run",
        "CREATE TABLE vault.evil (x int)",
    ] {
        let err = sqlx::query(stmt).execute(&app).await.expect_err(stmt);
        let code = err
            .as_database_error()
            .and_then(|e| e.code())
            .map(|c| c.to_string());
        assert!(matches!(code.as_deref(), Some("42501")), "{stmt}: {err}"); // insufficient_privilege
    }
}

#[sqlx::test(migrations = false)]
#[ignore = "needs Postgres from `devenv up`"]
async fn check_migrations_rejects_unmigrated_database(pool: PgPool) {
    let err = vs_vault::check_migrations(&pool).await.unwrap_err();
    assert!(err.to_string().contains("vs-api migrate"), "{err}");
}

fn page_result(m: &DocumentMeta, r: &RunMeta) -> PageResult {
    let page_hk = HashKey::page(&m.document_hk, 1);
    PageResult {
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
        blocks: vec![
            block(&page_hk, r.run_id, 0, Some("org@layout@2026-01")),
            block(&page_hk, r.run_id, 1, None),
        ],
    }
}

fn stage2_fixture(m: &DocumentMeta, r: &RunMeta) -> (StitchedDocument, ExtractionOutput) {
    let page = page_result(m, r);
    let b0 = page.blocks[0].block_hk.clone();
    let b1 = page.blocks[1].block_hk.clone();
    let doc = StitchedDocument {
        document_hk: m.document_hk.clone(),
        run_id: r.run_id,
        blocks: page
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| DocBlock {
                block: b.clone(),
                page_no: 1,
                page_hk: page.page_hk.clone(),
                global_order: i as u32,
            })
            .collect(),
        furniture: HashSet::new(),
        sections: vec![],
        links: vec![Link {
            kind: LinkKind::ContinuesOn,
            from: b0.clone(),
            to: b1.clone(),
            confidence: 0.9,
            header_repeated: false,
        }],
        missing_pages: vec![],
    };
    let prov = page.blocks[0].provenance.clone();
    let ext = ExtractionOutput {
        mentions: vec![
            Mention {
                id: "m1".into(),
                block_hk: b0.clone(),
                char_start: 0,
                char_end: 5,
                surface: "block".into(),
                raw_type: "Method".into(),
                confidence: 0.9,
            },
            Mention {
                id: "m2".into(),
                block_hk: b0.clone(),
                char_start: 6,
                char_end: 7,
                surface: "0".into(),
                raw_type: "Method".into(),
                confidence: 0.8,
            },
            Mention {
                id: "m3".into(),
                block_hk: b1.clone(),
                char_start: 0,
                char_end: 5,
                surface: "block".into(),
                raw_type: "Dataset".into(),
                confidence: 0.7,
            },
        ],
        entities: vec![
            DocEntity {
                local_id: "e1".into(),
                name: "Block".into(),
                raw_type: "Method".into(),
                type_id: Some("method".into()),
                aliases: vec!["blk".into()],
                mentions: vec!["m1".into(), "m2".into()],
                entity_hk: Some(HashKey::entity(&TenantId("t1".into()), "block")),
            },
            DocEntity {
                local_id: "e2".into(),
                name: "Block one".into(),
                raw_type: "Dataset".into(),
                type_id: None,
                aliases: vec![],
                mentions: vec!["m3".into()],
                entity_hk: None,
            },
        ],
        assertions: vec![Assertion {
            local_id: "a1".into(),
            predicate: "uses".into(),
            predicate_id: Some("uses".into()),
            kind: AssertionKind::Binary,
            args: vec![
                Arg {
                    role: "subject".into(),
                    value: ArgValue::Entity {
                        local_id: "e1".into(),
                    },
                },
                Arg {
                    role: "object".into(),
                    value: ArgValue::Entity {
                        local_id: "e2".into(),
                    },
                },
                Arg {
                    role: "metric".into(),
                    value: ArgValue::Literal {
                        value: "0.5".into(),
                        unit: Some("F1".into()),
                    },
                },
            ],
            qualifiers: Qualifiers::default(),
            evidence: vec![
                Evidence::Span {
                    block_hk: b0.clone(),
                    char_start: 0,
                    char_end: 7,
                },
                Evidence::Span {
                    block_hk: b1.clone(),
                    char_start: 0,
                    char_end: 7,
                },
                Evidence::Region {
                    page_hk: page.page_hk.clone(),
                    bbox: BBox::new(0.1, 0.1, 0.5, 0.5).unwrap(),
                },
            ],
            confidence: 0.8,
            verified: true,
            provenance_idx: 0,
        }],
        provenance: vec![prov],
        dropped_ungrounded: 0,
        dropped_unverified: 0,
    };
    (doc, ext)
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn write_stage2_is_idempotent_and_versions_satellites(pool: PgPool) {
    let v = PgVault { pool: pool.clone() };
    let (r, m) = (run(), meta(1));
    v.write_document(&r, &spec(), &m).await.unwrap();
    v.write_page(&page_result(&m, &r), Some("rule-1"))
        .await
        .unwrap();

    let (doc, mut ext) = stage2_fixture(&m, &r);
    v.write_stage2(&r, &spec(), &doc, &ext, JobState::Done)
        .await
        .unwrap();
    v.write_stage2(&r, &spec(), &doc, &ext, JobState::Done)
        .await
        .unwrap(); // retry

    assert_eq!(count(&pool, "vault.lnk_block_block").await, 1);
    assert_eq!(count(&pool, "vault.sat_stitch_link").await, 1);
    assert_eq!(count(&pool, "vault.hub_entity").await, 2);
    assert_eq!(count(&pool, "vault.sat_entity_label").await, 2);
    assert_eq!(count(&pool, "vault.lnk_mention").await, 2); // (b0,e1) and (b1,e2)
    assert_eq!(count(&pool, "vault.sat_mention").await, 3); // multi-active: two mentions on (b0,e1)
    assert_eq!(count(&pool, "vault.hub_assertion").await, 1);
    assert_eq!(count(&pool, "vault.sat_assertion_detail").await, 1);
    assert_eq!(count(&pool, "vault.lnk_assertion_arg").await, 2);
    assert_eq!(count(&pool, "vault.lnk_assertion_evidence").await, 3); // b0, b1, page
    assert_eq!(count(&pool, "vault.sat_evidence").await, 3);
    // sat_run: processing (ingest) + done (stage 2).
    assert_eq!(count(&pool, "vault.sat_run").await, 2);
    let state: String =
        sqlx::query_scalar("SELECT state FROM vault.sat_run ORDER BY load_dts DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "done");

    let resolved: String =
        sqlx::query_scalar("SELECT entity_hk FROM vault.hub_entity WHERE canonical_key = 'block'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        resolved.trim(),
        HashKey::entity(&TenantId("t1".into()), "block").as_str()
    );
    let local: String = sqlx::query_scalar(
        "SELECT canonical_key FROM vault.hub_entity WHERE canonical_key LIKE 'local:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(local, vs_vault::local_entity_key(&m.document_hk, "e2"));
    let region: i64 = sqlx::query_scalar("SELECT count(*) FROM vault.lnk_assertion_evidence WHERE block_hk IS NULL AND page_hk IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(region, 1);
    let literals: serde_json::Value =
        sqlx::query_scalar("SELECT literals FROM vault.sat_assertion_detail")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(literals[0]["role"], "metric");

    // A changed confidence adds a satellite row; hubs and links stay.
    ext.assertions[0].confidence = 0.95;
    ext.mentions[0].confidence = 0.95;
    v.write_stage2(&r, &spec(), &doc, &ext, JobState::Done)
        .await
        .unwrap();
    assert_eq!(count(&pool, "vault.hub_assertion").await, 1);
    assert_eq!(count(&pool, "vault.sat_assertion_detail").await, 2);
    assert_eq!(count(&pool, "vault.sat_mention").await, 5); // the (b0,e1) group re-inserted as a whole
    assert_eq!(count(&pool, "vault.lnk_mention").await, 2);

    // The API's summary reads it back.
    let ops = Ops { pool: pool.clone() };
    let sum = ops
        .document_summary(&spec().tenant, &m.document_hk, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (sum.run_id, sum.entities, sum.assertions),
        (r.run_id.0, 2, 1)
    );
    assert_eq!(sum.pages.len(), 1);
    assert_eq!(sum.blocks_by_kind.get("paragraph"), Some(&2));
    assert_eq!(sum.run_state.as_deref(), Some("done"));
    assert!(ops
        .document_summary(&TenantId("other".into()), &m.document_hk, None)
        .await
        .unwrap()
        .is_none());
    assert!(ops
        .document_summary(&spec().tenant, &m.document_hk, Some(RunId::new()))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        ops.document_run(&spec().tenant, &m.document_hk, Some(r.run_id))
            .await
            .unwrap(),
        Some(r.run_id)
    );
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn batches_jobs_and_timings(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let s = spec();
    let batch = Uuid::now_v7();
    ops.create_batch(batch, &s.tenant, 3).await.unwrap();
    let jobs: Vec<NewJob> = (0..3)
        .map(|_| NewJob {
            job_id: Uuid::now_v7(),
            run_id: RunId::new(),
            spec: s.clone(),
            document_hk: None,
        })
        .collect();
    ops.create_jobs(Some(batch), &jobs).await.unwrap();
    ops.set_state(jobs[0].job_id, JobState::Failed, Some("boom"))
        .await
        .unwrap();
    ops.set_state(jobs[0].job_id, JobState::Failed, None)
        .await
        .unwrap(); // same state again: no extra log row

    let b = ops
        .get_batch(&s.tenant, batch, 0, 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((b.documents, b.total_jobs, b.jobs.len()), (3, 3, 2));
    assert_eq!(b.counts.get("received"), Some(&2));
    assert_eq!(b.counts.get("failed"), Some(&1));
    let b2 = ops
        .get_batch(&s.tenant, batch, 1, 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b2.jobs.len(), 1);
    assert!(ops
        .get_batch(&TenantId("other".into()), batch, 0, 10)
        .await
        .unwrap()
        .is_none());

    let t = ops
        .job_timings(&s.tenant, jobs[0].job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        t.states
            .iter()
            .map(|x| x.state.as_str())
            .collect::<Vec<_>>(),
        vec!["received", "failed"]
    );

    // current_run follows the newest job of a document.
    let doc = meta(2).document_hk;
    assert!(ops.current_run(&s.tenant, &doc).await.unwrap().is_none());
    let j = Uuid::now_v7();
    ops.create_job_for_document(j, None, RunId::new(), &s, Some(&doc))
        .await
        .unwrap();
    let cur = ops.current_run(&s.tenant, &doc).await.unwrap().unwrap();
    assert_eq!((cur.job_id, cur.state.as_str()), (j, "received"));
    let row = ops.get_job(&s.tenant, j).await.unwrap().unwrap();
    assert_eq!(row.document_hk.as_deref(), Some(doc.as_str()));
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn idempotency_keys_round_trip(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let t = TenantId("t1".into());
    assert!(ops.get_idempotent(&t, "k").await.unwrap().is_none());
    ops.put_idempotent(
        &t,
        "k",
        &serde_json::json!({ "status": 202, "body": { "job_id": 1 } }),
    )
    .await
    .unwrap();
    // A live entry is not overwritten.
    ops.put_idempotent(&t, "k", &serde_json::json!({ "status": 500 }))
        .await
        .unwrap();
    let v = ops.get_idempotent(&t, "k").await.unwrap().unwrap();
    assert_eq!(v["status"], 202);
    assert!(ops
        .get_idempotent(&TenantId("t2".into()), "k")
        .await
        .unwrap()
        .is_none());
    sqlx::query("UPDATE ops.idempotency_keys SET created_at = now() - interval '25 hours'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(ops.get_idempotent(&t, "k").await.unwrap().is_none());
    assert_eq!(ops.purge_idempotency_keys().await.unwrap(), 1);
}

fn dag_list(version: &str) -> vs_router::DagList {
    serde_json::from_value(serde_json::json!({
        "version": version,
        "created_by": "test",
        "dags": [{ "id": "d", "version": "1", "output": "p",
                   "nodes": [{ "id": "p", "step": "page.parse_e2e", "inputs": ["$page_image"], "params": null }] }],
        "routing": { "default_dag": "d", "low_confidence_dag": "d", "min_classifier_confidence": 0.5, "rules": [] }
    }))
    .unwrap()
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn dag_lists_one_active_per_scope(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    let t = TenantId("t1".into());
    assert!(ops
        .dag_list_insert_candidate("g1", None, &dag_list("g1"), Some("adapt"))
        .await
        .unwrap());
    assert!(!ops
        .dag_list_insert_candidate("g1", None, &dag_list("g1"), None)
        .await
        .unwrap()); // duplicate version
    assert!(ops
        .dag_list_insert_candidate("g2", None, &dag_list("g2"), None)
        .await
        .unwrap());
    assert!(ops
        .dag_list_insert_candidate("t1-a", Some(&t), &dag_list("t1-a"), None)
        .await
        .unwrap());

    assert!(ops.active_dag_list(&t).await.unwrap().is_none());
    assert!(ops.dag_list_activate("g1", None).await.unwrap());
    assert!(!ops.dag_list_activate("g1", Some(&t)).await.unwrap()); // wrong scope
    assert!(!ops.dag_list_activate("nope", None).await.unwrap());
    assert_eq!(ops.active_dag_list(&t).await.unwrap().unwrap().0, "g1");
    assert!(ops.dag_list_activate("g2", None).await.unwrap());
    let rows = ops.dag_lists_list(&t).await.unwrap();
    let status = |v: &str| rows.iter().find(|r| r.version == v).unwrap().status.clone();
    assert_eq!(
        (
            status("g1").as_str(),
            status("g2").as_str(),
            status("t1-a").as_str()
        ),
        ("retired", "active", "candidate")
    );

    // The tenant's own active list wins over the global one.
    assert!(ops.dag_list_activate("t1-a", Some(&t)).await.unwrap());
    assert_eq!(ops.active_dag_list(&t).await.unwrap().unwrap().0, "t1-a");
    assert_eq!(
        ops.active_dag_list(&TenantId("t2".into()))
            .await
            .unwrap()
            .unwrap()
            .0,
        "g2"
    );
    assert_eq!(
        ops.active_dag_list_version(&t).await.unwrap().as_deref(),
        Some("t1-a")
    );
    assert_eq!(ops.dag_list_get("g2").await.unwrap().unwrap().version, "g2");
    assert!(ops.dag_list_get("nope").await.unwrap().is_none());
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn ontology_rows(pool: PgPool) {
    let ops = Ops { pool: pool.clone() };
    assert!(ops.entity_types().await.unwrap().is_empty());
    let seeded = ops.relation_types().await.unwrap();
    assert!(seeded
        .iter()
        .any(|r| r.predicate_id == "result" && r.roles.contains(&"metric".to_string())));
    ops.insert_pending_entity_type("method", "Method")
        .await
        .unwrap();
    ops.insert_pending_entity_type("method", "Method")
        .await
        .unwrap();
    ops.insert_pending_relation_type(
        "outperforms",
        "outperforms",
        &["subject".into(), "baseline".into()],
    )
    .await
    .unwrap();
    let et = ops.entity_types().await.unwrap();
    assert_eq!((et.len(), et[0].review_status.as_str()), (1, "pending"));
    let rt = ops.relation_types().await.unwrap();
    let o = rt.iter().find(|r| r.predicate_id == "outperforms").unwrap();
    assert_eq!((o.review_status.as_str(), o.roles.len()), ("pending", 2));
}
