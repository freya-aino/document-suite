//! Data Vault 2.0 writer. See `SPEC.md`.

pub mod ops;

use std::collections::BTreeSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use vs_core::{split_ref, HashKey, JobSpec, PageResult, RunId};
use vs_extract::ExtractionOutput;
use vs_stitch::StitchedDocument;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

/// Metadata known after ingest.
#[derive(Debug, Clone)]
pub struct DocumentMeta {
    pub document_hk: HashKey,
    pub content_sha256: String,
    pub filename: Option<String>,
    pub size_bytes: i64,
    pub format: String,
    pub page_count: u32,
}

/// The run as known at ingest. `started_at` must be stable across retries (job creation
/// time), otherwise every retry would add a `sat_run` row.
#[derive(Debug, Clone)]
pub struct RunMeta {
    pub run_id: RunId,
    pub dag_list_version: String,
    pub started_at: DateTime<Utc>,
}

#[async_trait]
pub trait VaultWriter: Send + Sync {
    async fn write_document(&self, run: &RunMeta, spec: &JobSpec, meta: &DocumentMeta) -> Result<(), VaultError>;
    async fn write_page(&self, page: &PageResult, rule_id: Option<&str>) -> Result<(), VaultError>;
    async fn write_stage2(&self, doc: &StitchedDocument, ext: &ExtractionOutput) -> Result<(), VaultError>;
}

pub struct PgVault {
    pub pool: PgPool,
}

pub fn record_source(component: &str, what: &str, model: Option<&str>) -> String {
    format!("vs/{component}/{what}/{}", model.unwrap_or("cpu"))
}

/// SHA-256 over a satellite's descriptive columns (passed as one JSON value; object keys
/// serialise sorted, so the result is stable).
fn hash_diff(descriptive: &Value) -> String {
    HashKey::of(&[&descriptive.to_string()]).to_string()
}

/// serde name of a unit enum variant (`Priority::Batch` → `"batch"`).
pub(crate) fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        other => format!("{other:?}"),
    }
}

/// `WHERE` clause for satellite inserts selecting from a row source `v` with columns
/// `<parent>` and `hash_diff`: insert only if the parent's latest satellite row has a
/// different hash_diff (or none exists).
const LATEST_DIFFERS: &str = "NOT EXISTS (SELECT 1 FROM (SELECT s.hash_diff FROM {table} s WHERE s.{parent} = v.{parent} \
     ORDER BY s.load_dts DESC LIMIT 1) l WHERE l.hash_diff = v.hash_diff)";

fn latest_differs(table: &str, parent: &str) -> String {
    LATEST_DIFFERS.replace("{table}", table).replace("{parent}", parent)
}

#[async_trait]
impl VaultWriter for PgVault {
    async fn write_document(&self, run: &RunMeta, spec: &JobSpec, meta: &DocumentMeta) -> Result<(), VaultError> {
        let now = Utc::now();
        let src = record_source("ingest", &run.dag_list_version, None);
        let doc_hk = meta.document_hk.as_str();
        let run_hk = HashKey::run(&run.run_id);
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO vault.hub_document (document_hk, tenant_id, content_sha256, load_dts, record_source)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(doc_hk)
        .bind(spec.tenant.as_str())
        .bind(&meta.content_sha256)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;

        let desc = json!([spec.external_ref, meta.filename, meta.size_bytes, meta.format, spec.language.as_str(), meta.page_count]);
        sqlx::query(&format!(
            "INSERT INTO vault.sat_document_meta
               (document_hk, load_dts, hash_diff, record_source, external_ref, filename, size_bytes, format, language_hint, page_count)
             SELECT v.document_hk, $2, v.hash_diff, $4, $5, $6, $7, $8, $9, $10
             FROM (SELECT $1::text AS document_hk, $3::text AS hash_diff) v
             WHERE {}",
            latest_differs("vault.sat_document_meta", "document_hk")
        ))
        .bind(doc_hk)
        .bind(now)
        .bind(hash_diff(&desc))
        .bind(&src)
        .bind(&spec.external_ref)
        .bind(&meta.filename)
        .bind(meta.size_bytes)
        .bind(&meta.format)
        .bind(spec.language.as_str())
        .bind(meta.page_count as i32)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO vault.hub_run (run_hk, run_id, load_dts, record_source) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(run_hk.as_str())
        .bind(run.run_id.0)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;

        insert_sat_run(&mut tx, &run_hk, now, &src, &run.dag_list_version, spec, "processing", Some(run.started_at), None).await?;

        sqlx::query(
            "INSERT INTO vault.lnk_run_document (lnk_hk, run_hk, document_hk, load_dts, record_source)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(HashKey::link(&[&run_hk, &meta.document_hk], None).as_str())
        .bind(run_hk.as_str())
        .bind(doc_hk)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;

        let page_nos: Vec<i32> = (1..=meta.page_count as i32).collect();
        let page_hks: Vec<String> = page_nos.iter().map(|&n| HashKey::page(&meta.document_hk, n as u32).to_string()).collect();
        let lnk_hks: Vec<String> = page_nos
            .iter()
            .map(|&n| HashKey::link(&[&meta.document_hk, &HashKey::page(&meta.document_hk, n as u32)], None).to_string())
            .collect();
        sqlx::query(
            "INSERT INTO vault.hub_page (page_hk, document_hk, page_no, load_dts, record_source)
             SELECT p.page_hk, $3, p.page_no, $4, $5 FROM UNNEST($1::text[], $2::int[]) AS p(page_hk, page_no)
             ON CONFLICT DO NOTHING",
        )
        .bind(&page_hks)
        .bind(&page_nos)
        .bind(doc_hk)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO vault.lnk_document_page (lnk_hk, document_hk, page_hk, load_dts, record_source)
             SELECT l.lnk_hk, $3, l.page_hk, $4, $5 FROM UNNEST($1::text[], $2::text[]) AS l(lnk_hk, page_hk)
             ON CONFLICT DO NOTHING",
        )
        .bind(&lnk_hks)
        .bind(&page_hks)
        .bind(doc_hk)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn write_page(&self, page: &PageResult, rule_id: Option<&str>) -> Result<(), VaultError> {
        let now = Utc::now();
        let page_src = record_source("classify", &page.dag_ref, None);
        let dag_hk = HashKey::dag(&page.dag_ref);
        let mut tx = self.pool.begin().await?;

        let profile = serde_json::to_value(&page.profile).unwrap_or(Value::Null);
        let desc = json!([page.run_id.0, profile, page.dag_ref, rule_id, page.render_width_px, page.render_height_px]);
        sqlx::query(&format!(
            "INSERT INTO vault.sat_page_profile
               (page_hk, load_dts, hash_diff, record_source, run_id, profile, dag_ref, rule_id, render_width_px, render_height_px)
             SELECT v.page_hk, $2, v.hash_diff, $4, $5, $6, $7, $8, $9, $10
             FROM (SELECT $1::text AS page_hk, $3::text AS hash_diff) v
             WHERE {}",
            latest_differs("vault.sat_page_profile", "page_hk")
        ))
        .bind(page.page_hk.as_str())
        .bind(now)
        .bind(hash_diff(&desc))
        .bind(&page_src)
        .bind(page.run_id.0)
        .bind(&profile)
        .bind(&page.dag_ref)
        .bind(rule_id)
        .bind(page.render_width_px as i32)
        .bind(page.render_height_px as i32)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO vault.hub_dag (dag_hk, dag_id, version, load_dts, record_source) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING",
        )
        .bind(dag_hk.as_str())
        .bind(split_ref(&page.dag_ref).0)
        .bind(split_ref(&page.dag_ref).1)
        .bind(now)
        .bind(&page_src)
        .execute(&mut *tx)
        .await?;

        let models: BTreeSet<&str> = page.blocks.iter().filter_map(|b| b.provenance.model_ref.as_deref()).collect();
        let model_hks: Vec<String> = models.iter().map(|m| HashKey::model(m).to_string()).collect();
        let model_ids: Vec<&str> = models.iter().map(|m| split_ref(m).0).collect();
        let model_revs: Vec<&str> = models.iter().map(|m| split_ref(m).1).collect();
        sqlx::query(
            "INSERT INTO vault.hub_model (model_hk, model_id, revision, load_dts, record_source)
             SELECT m.model_hk, m.model_id, m.revision, $4, $5 FROM UNNEST($1::text[], $2::text[], $3::text[]) AS m(model_hk, model_id, revision)
             ON CONFLICT DO NOTHING",
        )
        .bind(&model_hks)
        .bind(&model_ids)
        .bind(&model_revs)
        .bind(now)
        .bind(&page_src)
        .execute(&mut *tx)
        .await?;

        let b = BlockColumns::of(page, &dag_hk);
        sqlx::query(
            "INSERT INTO vault.hub_block (block_hk, page_hk, run_id, block_index, load_dts, record_source)
             SELECT v.block_hk, $4, $5, v.block_index, $6, v.record_source
             FROM UNNEST($1::text[], $2::int[], $3::text[]) AS v(block_hk, block_index, record_source)
             ON CONFLICT DO NOTHING",
        )
        .bind(&b.block_hk)
        .bind(&b.index)
        .bind(&b.record_source)
        .bind(page.page_hk.as_str())
        .bind(page.run_id.0)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO vault.lnk_page_block (lnk_hk, page_hk, block_hk, load_dts, record_source)
             SELECT v.lnk_hk, $4, v.block_hk, $5, v.record_source
             FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(lnk_hk, block_hk, record_source)
             ON CONFLICT DO NOTHING",
        )
        .bind(&b.page_lnk_hk)
        .bind(&b.block_hk)
        .bind(&b.record_source)
        .bind(page.page_hk.as_str())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(&format!(
            "INSERT INTO vault.sat_block_content
               (block_hk, load_dts, hash_diff, record_source, kind, bbox, reading_order, global_order, text, content, level, confidence, furniture)
             SELECT v.block_hk, $1, v.hash_diff, v.record_source, v.kind, v.bbox, v.reading_order, NULL, v.text, v.content, v.level, v.confidence, false
             FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::jsonb[], $7::int[], $8::text[], $9::jsonb[], $10::smallint[], $11::real[])
               AS v(block_hk, hash_diff, record_source, kind, bbox, reading_order, text, content, level, confidence)
             WHERE {}",
            latest_differs("vault.sat_block_content", "block_hk")
        ))
        .bind(now)
        .bind(&b.block_hk)
        .bind(&b.content_hash_diff)
        .bind(&b.record_source)
        .bind(&b.kind)
        .bind(&b.bbox)
        .bind(&b.reading_order)
        .bind(&b.text)
        .bind(&b.content)
        .bind(&b.level)
        .bind(&b.confidence)
        .execute(&mut *tx)
        .await?;

        sqlx::query(&format!(
            "INSERT INTO vault.sat_block_provenance (block_hk, load_dts, hash_diff, record_source, provenance)
             SELECT v.block_hk, $1, v.hash_diff, v.record_source, v.provenance
             FROM UNNEST($2::text[], $3::text[], $4::text[], $5::jsonb[]) AS v(block_hk, hash_diff, record_source, provenance)
             WHERE {}",
            latest_differs("vault.sat_block_provenance", "block_hk")
        ))
        .bind(now)
        .bind(&b.block_hk)
        .bind(&b.provenance_hash_diff)
        .bind(&b.record_source)
        .bind(&b.provenance)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO vault.lnk_block_dag_model (lnk_hk, block_hk, dag_hk, model_hk, node_id, load_dts, record_source)
             SELECT v.lnk_hk, v.block_hk, $6, v.model_hk, v.node_id, $7, v.record_source
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[]) AS v(lnk_hk, block_hk, model_hk, node_id, record_source)
             ON CONFLICT DO NOTHING",
        )
        .bind(&b.dag_model_lnk_hk)
        .bind(&b.block_hk)
        .bind(&b.model_hk)
        .bind(&b.node_id)
        .bind(&b.record_source)
        .bind(dag_hk.as_str())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn write_stage2(&self, _doc: &StitchedDocument, _ext: &ExtractionOutput) -> Result<(), VaultError> {
        Err(VaultError::NotImplemented("write_stage2"))
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_sat_run(
    tx: &mut sqlx::PgConnection,
    run_hk: &HashKey,
    now: DateTime<Utc>,
    src: &str,
    dag_list_version: &str,
    spec: &JobSpec,
    state: &str,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
) -> Result<(), VaultError> {
    let (profile, priority) = (enum_str(&spec.profile), enum_str(&spec.priority));
    let desc = json!([dag_list_version, profile, priority, state, started_at, finished_at]);
    sqlx::query(&format!(
        "INSERT INTO vault.sat_run (run_hk, load_dts, hash_diff, record_source, dag_list_version, profile, priority, state, started_at, finished_at)
         SELECT v.run_hk, $2, v.hash_diff, $4, $5, $6, $7, $8, $9, $10
         FROM (SELECT $1::text AS run_hk, $3::text AS hash_diff) v
         WHERE {}",
        latest_differs("vault.sat_run", "run_hk")
    ))
    .bind(run_hk.as_str())
    .bind(now)
    .bind(hash_diff(&desc))
    .bind(src)
    .bind(dag_list_version)
    .bind(profile)
    .bind(priority)
    .bind(state)
    .bind(started_at)
    .bind(finished_at)
    .execute(tx)
    .await?;
    Ok(())
}

/// Per-block column arrays for the `UNNEST` inserts of `write_page`.
struct BlockColumns {
    block_hk: Vec<String>,
    index: Vec<i32>,
    record_source: Vec<String>,
    page_lnk_hk: Vec<String>,
    kind: Vec<String>,
    bbox: Vec<Value>,
    reading_order: Vec<i32>,
    text: Vec<String>,
    content: Vec<Value>,
    level: Vec<Option<i16>>,
    confidence: Vec<f32>,
    content_hash_diff: Vec<String>,
    provenance: Vec<Value>,
    provenance_hash_diff: Vec<String>,
    model_hk: Vec<Option<String>>,
    node_id: Vec<String>,
    dag_model_lnk_hk: Vec<String>,
}

impl BlockColumns {
    fn of(page: &PageResult, dag_hk: &HashKey) -> Self {
        let n = page.blocks.len();
        let mut c = BlockColumns {
            block_hk: Vec::with_capacity(n),
            index: Vec::with_capacity(n),
            record_source: Vec::with_capacity(n),
            page_lnk_hk: Vec::with_capacity(n),
            kind: Vec::with_capacity(n),
            bbox: Vec::with_capacity(n),
            reading_order: Vec::with_capacity(n),
            text: Vec::with_capacity(n),
            content: Vec::with_capacity(n),
            level: Vec::with_capacity(n),
            confidence: Vec::with_capacity(n),
            content_hash_diff: Vec::with_capacity(n),
            provenance: Vec::with_capacity(n),
            provenance_hash_diff: Vec::with_capacity(n),
            model_hk: Vec::with_capacity(n),
            node_id: Vec::with_capacity(n),
            dag_model_lnk_hk: Vec::with_capacity(n),
        };
        for b in &page.blocks {
            let model = b.provenance.model_ref.as_deref();
            let model_hk = model.map(HashKey::model);
            let kind = enum_str(&b.kind);
            let bbox = serde_json::to_value(b.bbox).unwrap_or(Value::Null);
            let content = serde_json::to_value(&b.content).unwrap_or(Value::Null);
            let provenance = serde_json::to_value(&b.provenance).unwrap_or(Value::Null);
            let level = b.level.map(i16::from);

            let mut lnk_parents = vec![&b.block_hk, dag_hk];
            lnk_parents.extend(model_hk.as_ref());
            c.dag_model_lnk_hk.push(HashKey::link(&lnk_parents, Some(&b.provenance.node_id)).to_string());
            c.page_lnk_hk.push(HashKey::link(&[&page.page_hk, &b.block_hk], None).to_string());
            c.content_hash_diff.push(hash_diff(&json!([kind, bbox, b.reading_order, b.text, content, level, b.confidence])));
            c.provenance_hash_diff.push(hash_diff(&provenance));
            c.record_source.push(record_source("page", &page.dag_ref, model));
            c.block_hk.push(b.block_hk.to_string());
            c.index.push(b.index as i32);
            c.kind.push(kind);
            c.bbox.push(bbox);
            c.reading_order.push(b.reading_order as i32);
            c.text.push(b.text.clone());
            c.content.push(content);
            c.level.push(level);
            c.confidence.push(b.confidence);
            c.provenance.push(provenance);
            c.model_hk.push(model_hk.map(|h| h.to_string()));
            c.node_id.push(b.provenance.node_id.clone());
        }
        c
    }
}
