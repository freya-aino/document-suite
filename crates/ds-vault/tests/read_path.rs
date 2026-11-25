//! The read path's indexes and the shape of the queries that use them, against Postgres
//! from `devenv --profile stack up` (`DATABASE_URL`). Run with
//! `cargo test -p ds-vault --test read_path -- --ignored`.
//!
//! Why this file exists: every table in migration 0002 was indexed on its own hash key
//! and on nothing else, so every traversal *away* from a key was a sequential scan, and
//! `READING_ORDER_QUERY` ranked every placement row in the database before narrowing to
//! one document. None of that is visible in a test that processes one document — it is
//! only visible in a plan — so these two tests read the plan.

use sqlx::{PgPool, Row};

/// Every index the documented read patterns (`docs/STORES.md`) depend on, by name, so
/// dropping one in a later migration fails here rather than in a plan nobody looks at.
const READ_PATH_INDEXES: &[&str] = &[
    "lnk_document_page_document_idx",
    "lnk_page_block_page_idx",
    "lnk_run_document_document_idx",
    "lnk_run_document_run_idx",
    "lnk_block_block_from_idx",
    "lnk_block_block_to_idx",
    "lnk_mention_block_idx",
    "lnk_mention_entity_idx",
    "lnk_assertion_arg_assertion_idx",
    "lnk_assertion_arg_entity_idx",
    "lnk_assertion_evidence_assertion_idx",
    "lnk_assertion_evidence_block_idx",
    "lnk_block_dag_model_block_idx",
    "hub_page_document_idx",
    "hub_block_page_idx",
    "hub_block_run_idx",
    "hub_assertion_run_idx",
];

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn every_link_and_hub_the_read_path_traverses_is_indexed(pool: PgPool) {
    let present: Vec<String> = sqlx::query("SELECT indexname FROM pg_indexes WHERE schemaname = 'vault'")
        .fetch_all(&pool)
        .await
        .expect("index list")
        .into_iter()
        .map(|r| r.get::<String, _>("indexname"))
        .collect();

    let missing: Vec<&str> = READ_PATH_INDEXES.iter().copied().filter(|i| !present.iter().any(|p| p == i)).collect();
    assert!(missing.is_empty(), "missing read-path indexes: {missing:?}");
}

/// Four documents of a thousand blocks each, so a plan that scans the satellite costs
/// four times what one that walks a document costs — and the difference is visible.
///
/// The keys are `md5(name) || md5(name || 'x')`: 64 hex characters, which is the shape
/// `CHAR(64)` wants, derived from a name the test can recompute. Spelled out at every
/// site rather than wrapped in a helper function, because a `pg_temp` function lives in
/// one session and `sqlx` hands out a different connection per query.
async fn seed(pool: &PgPool, docs: i32, pages: i32, blocks: i32) {
    let inserts = [
        "INSERT INTO vault.hub_document (document_hk, tenant_id, content_sha256, load_dts, record_source)
         SELECT (md5('doc' || d) || md5('doc' || d || 'x')), 't1', md5(d::text) || md5(d::text), now(), 'seed'
         FROM generate_series(1, $1) d",
        "INSERT INTO vault.hub_run (run_hk, run_id, load_dts, record_source)
         SELECT (md5('run' || d) || md5('run' || d || 'x')), ('00000000-0000-7000-8000-' || lpad(to_hex(d), 12, '0'))::uuid, now(), 'seed' FROM generate_series(1, $1) d",
        "INSERT INTO vault.lnk_run_document (lnk_hk, run_hk, document_hk, load_dts, record_source)
         SELECT (md5('rd' || d) || md5('rd' || d || 'x')), (md5('run' || d) || md5('run' || d || 'x')), (md5('doc' || d) || md5('doc' || d || 'x')), now(), 'seed'
         FROM generate_series(1, $1) d",
        "INSERT INTO vault.hub_page (page_hk, document_hk, page_no, load_dts, record_source)
         SELECT (md5('pg' || d || '/' || p) || md5('pg' || d || '/' || p || 'x')), (md5('doc' || d) || md5('doc' || d || 'x')), p, now(), 'seed'
         FROM generate_series(1, $1) d, generate_series(1, $2) p",
        "INSERT INTO vault.hub_block (block_hk, page_hk, run_id, block_index, load_dts, record_source)
         SELECT (md5('bl' || d || '/' || p || '/' || b) || md5('bl' || d || '/' || p || '/' || b || 'x')), (md5('pg' || d || '/' || p) || md5('pg' || d || '/' || p || 'x')),
                ('00000000-0000-7000-8000-' || lpad(to_hex(d), 12, '0'))::uuid, b, now(), 'seed'
         FROM generate_series(1, $1) d, generate_series(1, $2) p, generate_series(1, $3) b",
        "INSERT INTO vault.sat_block_placement
             (block_hk, load_dts, hash_diff, record_source, global_order, furniture)
         SELECT (md5('bl' || d || '/' || p || '/' || b) || md5('bl' || d || '/' || p || '/' || b || 'x')), now(), md5('h' || b) || md5('h' || b),
                'seed', (p - 1) * $3 + b, b = $3
         FROM generate_series(1, $1) d, generate_series(1, $2) p, generate_series(1, $3) b",
    ];
    for q in inserts {
        sqlx::query(q).bind(docs).bind(pages).bind(blocks).execute(pool).await.expect("seed");
    }

    // Two thousand more documents with a run each and no pages, so the corpus is big
    // enough for the planner to have a choice about `vault.v_current_run`: on four rows
    // it reads the table whatever indexes exist, and the assertion below would say
    // nothing.
    for q in [
        "INSERT INTO vault.hub_document (document_hk, tenant_id, content_sha256, load_dts, record_source)
         SELECT md5('bulk' || d) || md5('bulk' || d || 'x'), 't1', md5('bulk' || d) || md5('bulk' || d), now(), 'seed'
         FROM generate_series(1, 2000) d",
        "INSERT INTO vault.hub_run (run_hk, run_id, load_dts, record_source)
         SELECT md5('bulkrun' || d) || md5('bulkrun' || d || 'x'),
                ('00000000-0000-7000-8000-' || lpad(to_hex(100000 + d), 12, '0'))::uuid, now(), 'seed'
         FROM generate_series(1, 2000) d",
        "INSERT INTO vault.lnk_run_document (lnk_hk, run_hk, document_hk, load_dts, record_source)
         SELECT md5('bulkrd' || d) || md5('bulkrd' || d || 'x'),
                md5('bulkrun' || d) || md5('bulkrun' || d || 'x'),
                md5('bulk' || d) || md5('bulk' || d || 'x'), now(), 'seed'
         FROM generate_series(1, 2000) d",
    ] {
        sqlx::query(q).execute(pool).await.expect("bulk seed");
    }
    sqlx::query("ANALYZE").execute(pool).await.expect("analyze");
}

/// The plan for one document must not scan the whole corpus.
///
/// Only `READING_ORDER_QUERY` is checked this way. The other documented reads are single
/// indexed lookups whose plan depends on how much is in the table — at test scale
/// Postgres rightly reads a small table rather than its index, and an assertion about
/// that would be measuring the planner. What this query had wrong was its *shape*: it
/// ranked every placement row in the database before narrowing to one document, and no
/// amount of data makes that shape acceptable, so the plan is the right place to hold
/// it. The indexes the others need are held by the test above.
#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_document_read_does_not_scan_the_corpus(pool: PgPool) {
    seed(&pool, 4, 10, 100).await;
    let document_hk: String = sqlx::query_scalar("SELECT md5('doc2') || md5('doc2x')").fetch_one(&pool).await.expect("hk");
    let run_id: uuid::Uuid = sqlx::query_scalar("SELECT ('00000000-0000-7000-8000-' || lpad(to_hex(2), 12, '0'))::uuid").fetch_one(&pool).await.expect("run");

    // The constant is this crate's own, not request data (see `latest_differs`).
    let explain_order = sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {}", ds_vault::READING_ORDER_QUERY));
    let order: serde_json::Value = sqlx::query_scalar(explain_order)
        .bind(&document_hk)
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("explain the reading-order query");
    assert!(
        !scans(&order, "sat_block_placement"),
        "READING_ORDER_QUERY scans the whole placement satellite:\n{order:#}"
    );
    assert!(!scans(&order, "hub_block"), "READING_ORDER_QUERY scans every block:\n{order:#}");

}

/// Does the plan read `relation` sequentially? A `Seq Scan` on a table the query means to
/// touch one document's worth of is the regression this guards against.
fn scans(plan: &serde_json::Value, relation: &str) -> bool {
    fn walk(n: &serde_json::Value, relation: &str) -> bool {
        if let Some(node) = n.as_object() {
            let seq = node.get("Node Type").and_then(|t| t.as_str()).is_some_and(|t| t.ends_with("Seq Scan"));
            let on = node.get("Relation Name").and_then(|r| r.as_str()) == Some(relation);
            if seq && on {
                return true;
            }
        }
        match n {
            serde_json::Value::Array(a) => a.iter().any(|c| walk(c, relation)),
            serde_json::Value::Object(o) => o.values().any(|c| walk(c, relation)),
            _ => false,
        }
    }
    walk(plan, relation)
}
