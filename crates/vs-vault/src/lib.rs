//! Data Vault 2.0 writer. See `SPEC.md`.

pub mod ontology;
pub mod ops;

pub use ontology::PgOntologyStore;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use vs_core::{split_ref, Evidence, HashKey, JobSpec, JobState, PageResult, RunId};
use vs_extract::{ArgValue, ExtractionOutput};
use vs_stitch::StitchedDocument;

/// `dag_ref` recorded for Stage 2 outputs (`record_source`, `hub_dag` is not used).
pub const STAGE2_REF: &str = "stage2@1";

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Runtime processes do not migrate (their role cannot change the schema); they check
/// that every embedded migration is applied with a matching checksum.
pub async fn check_migrations(pool: &PgPool) -> Result<(), VaultError> {
    let applied: Vec<(i64, Vec<u8>)> = sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations WHERE success")
        .fetch_all(pool)
        .await
        .map_err(|e| VaultError::Schema(format!("cannot read migration history ({e}); run `vs-api migrate`")))?;
    for m in MIGRATOR.iter() {
        match applied.iter().find(|(v, _)| *v == m.version) {
            None => return Err(VaultError::Schema(format!("migration {} ({}) not applied; run `vs-api migrate`", m.version, m.description))),
            Some((_, sum)) if sum.as_slice() != m.checksum.as_ref() => {
                return Err(VaultError::Schema(format!("migration {} changed after it was applied", m.version)))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("schema: {0}")]
    Schema(String),
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
    /// Stage 2 output of one run, plus the final `sat_run` row (`final_state` is `Done`
    /// or `Partial`). The tenant comes from `spec`; `run` must be the one used for
    /// `write_document` (same `started_at`).
    async fn write_stage2(
        &self,
        run: &RunMeta,
        spec: &JobSpec,
        doc: &StitchedDocument,
        ext: &ExtractionOutput,
        final_state: JobState,
    ) -> Result<(), VaultError>;
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

/// serde name of a unit enum variant (`Priority::Batch` → `"batch"`), as stored in `ops`.
pub fn enum_name<T: Serialize>(v: &T) -> String {
    enum_str(v)
}

/// A `HashKey` from its 64-hex-character form (API path parameters). `None` if malformed.
pub fn hash_key_from_hex(hex: &str) -> Option<HashKey> {
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    serde_json::from_value(Value::String(hex.to_ascii_lowercase())).ok()
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

        sqlx::query("INSERT INTO vault.hub_run (run_hk, run_id, load_dts, record_source) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
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
        let lnk_hks: Vec<String> =
            page_nos.iter().map(|&n| HashKey::link(&[&meta.document_hk, &HashKey::page(&meta.document_hk, n as u32)], None).to_string()).collect();
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

    async fn write_stage2(
        &self,
        run: &RunMeta,
        spec: &JobSpec,
        doc: &StitchedDocument,
        ext: &ExtractionOutput,
        final_state: JobState,
    ) -> Result<(), VaultError> {
        let now = Utc::now();
        let src = record_source("doc", STAGE2_REF, None);
        let run_id = doc.run_id;
        let s2 = Stage2Columns::of(spec, doc, ext, &src);
        let mut tx = self.pool.begin().await?;

        // Stitch links.
        sqlx::query(
            "INSERT INTO vault.lnk_block_block (lnk_hk, from_block_hk, to_block_hk, link_kind, load_dts, record_source)
             SELECT v.lnk_hk, v.from_hk, v.to_hk, v.kind, $5, $6
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[]) AS v(lnk_hk, from_hk, to_hk, kind)
             ON CONFLICT DO NOTHING",
        )
        .bind(&s2.link_hk)
        .bind(&s2.link_from)
        .bind(&s2.link_to)
        .bind(&s2.link_kind)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO vault.sat_stitch_link (lnk_hk, load_dts, hash_diff, record_source, run_id, confidence)
             SELECT v.lnk_hk, $1, v.hash_diff, $4, $5, v.confidence
             FROM UNNEST($2::text[], $3::text[], $6::real[]) AS v(lnk_hk, hash_diff, confidence)
             WHERE {}",
            latest_differs("vault.sat_stitch_link", "lnk_hk")
        ))
        .bind(now)
        .bind(&s2.link_hk)
        .bind(&s2.link_hash_diff)
        .bind(&src)
        .bind(run_id.0)
        .bind(&s2.link_confidence)
        .execute(&mut *tx)
        .await?;

        // Entities.
        sqlx::query(
            "INSERT INTO vault.hub_entity (entity_hk, tenant_id, canonical_key, load_dts, record_source)
             SELECT v.entity_hk, $3, v.canonical_key, $4, $5
             FROM UNNEST($1::text[], $2::text[]) AS v(entity_hk, canonical_key)
             ON CONFLICT DO NOTHING",
        )
        .bind(&s2.entity_hk)
        .bind(&s2.entity_canonical_key)
        .bind(spec.tenant.as_str())
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO vault.sat_entity_label (entity_hk, load_dts, hash_diff, record_source, name, type_id, raw_type, aliases, review_status)
             SELECT v.entity_hk, $1, v.hash_diff, $4, v.name, v.type_id, v.raw_type, v.aliases, v.review_status
             FROM UNNEST($2::text[], $3::text[], $5::text[], $6::text[], $7::text[], $8::jsonb[], $9::text[])
               AS v(entity_hk, hash_diff, name, type_id, raw_type, aliases, review_status)
             WHERE {}",
            latest_differs("vault.sat_entity_label", "entity_hk")
        ))
        .bind(now)
        .bind(&s2.entity_hk)
        .bind(&s2.entity_hash_diff)
        .bind(&src)
        .bind(&s2.entity_name)
        .bind(&s2.entity_type_id)
        .bind(&s2.entity_raw_type)
        .bind(&s2.entity_aliases)
        .bind(&s2.entity_review_status)
        .execute(&mut *tx)
        .await?;

        // Mentions: one link per (block, entity); multi-active satellite rows per mention.
        sqlx::query(
            "INSERT INTO vault.lnk_mention (lnk_hk, block_hk, entity_hk, load_dts, record_source)
             SELECT v.lnk_hk, v.block_hk, v.entity_hk, $4, $5
             FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(lnk_hk, block_hk, entity_hk)
             ON CONFLICT DO NOTHING",
        )
        .bind(&s2.mention_lnk_hk)
        .bind(&s2.mention_block_hk)
        .bind(&s2.mention_entity_hk)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO vault.sat_mention (lnk_hk, load_dts, sub_seq, hash_diff, record_source, run_id, char_start, char_end, surface, confidence)
             SELECT v.lnk_hk, $1, v.sub_seq, v.hash_diff, $4, $5, v.char_start, v.char_end, v.surface, v.confidence
             FROM UNNEST($2::text[], $3::text[], $6::int[], $7::int[], $8::int[], $9::text[], $10::real[])
               AS v(lnk_hk, hash_diff, sub_seq, char_start, char_end, surface, confidence)
             WHERE {}",
            latest_differs("vault.sat_mention", "lnk_hk")
        ))
        .bind(now)
        .bind(&s2.sm_lnk_hk)
        .bind(&s2.sm_hash_diff)
        .bind(&src)
        .bind(run_id.0)
        .bind(&s2.sm_sub_seq)
        .bind(&s2.sm_char_start)
        .bind(&s2.sm_char_end)
        .bind(&s2.sm_surface)
        .bind(&s2.sm_confidence)
        .execute(&mut *tx)
        .await?;

        // Assertions.
        sqlx::query(
            "INSERT INTO vault.hub_assertion (assertion_hk, run_id, predicate, arg_keys, load_dts, record_source)
             SELECT v.assertion_hk, $4, v.predicate, v.arg_keys, $5, $6
             FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(assertion_hk, predicate, arg_keys)
             ON CONFLICT DO NOTHING",
        )
        .bind(&s2.assertion_hk)
        .bind(&s2.assertion_predicate)
        .bind(&s2.assertion_arg_keys)
        .bind(run_id.0)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO vault.sat_assertion_detail
               (assertion_hk, load_dts, hash_diff, record_source, predicate_raw, predicate_id, kind, qualifiers, literals, confidence, verified, provenance)
             SELECT v.assertion_hk, $1, v.hash_diff, v.record_source, v.predicate_raw, v.predicate_id, v.kind, v.qualifiers, v.literals,
                    v.confidence, v.verified, v.provenance
             FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::jsonb[], $9::jsonb[], $10::real[], $11::bool[], $12::jsonb[])
               AS v(assertion_hk, hash_diff, record_source, predicate_raw, predicate_id, kind, qualifiers, literals, confidence, verified, provenance)
             WHERE {}",
            latest_differs("vault.sat_assertion_detail", "assertion_hk")
        ))
        .bind(now)
        .bind(&s2.assertion_hk)
        .bind(&s2.assertion_hash_diff)
        .bind(&s2.assertion_record_source)
        .bind(&s2.assertion_predicate)
        .bind(&s2.assertion_predicate_id)
        .bind(&s2.assertion_kind)
        .bind(&s2.assertion_qualifiers)
        .bind(&s2.assertion_literals)
        .bind(&s2.assertion_confidence)
        .bind(&s2.assertion_verified)
        .bind(&s2.assertion_provenance)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO vault.lnk_assertion_arg (lnk_hk, assertion_hk, entity_hk, role, load_dts, record_source)
             SELECT v.lnk_hk, v.assertion_hk, v.entity_hk, v.role, $5, $6
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[]) AS v(lnk_hk, assertion_hk, entity_hk, role)
             ON CONFLICT DO NOTHING",
        )
        .bind(&s2.arg_lnk_hk)
        .bind(&s2.arg_assertion_hk)
        .bind(&s2.arg_entity_hk)
        .bind(&s2.arg_role)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;

        // Evidence: one link per (assertion, block | page); multi-active satellite rows per span/region.
        sqlx::query(
            "INSERT INTO vault.lnk_assertion_evidence (lnk_hk, assertion_hk, block_hk, page_hk, load_dts, record_source)
             SELECT v.lnk_hk, v.assertion_hk, v.block_hk, v.page_hk, $5, $6
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[]) AS v(lnk_hk, assertion_hk, block_hk, page_hk)
             ON CONFLICT DO NOTHING",
        )
        .bind(&s2.ev_lnk_hk)
        .bind(&s2.ev_assertion_hk)
        .bind(&s2.ev_block_hk)
        .bind(&s2.ev_page_hk)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO vault.sat_evidence (lnk_hk, load_dts, sub_seq, hash_diff, record_source, char_start, char_end, bbox)
             SELECT v.lnk_hk, $1, v.sub_seq, v.hash_diff, $4, v.char_start, v.char_end, v.bbox
             FROM UNNEST($2::text[], $3::text[], $5::int[], $6::int[], $7::int[], $8::jsonb[])
               AS v(lnk_hk, hash_diff, sub_seq, char_start, char_end, bbox)
             WHERE {}",
            latest_differs("vault.sat_evidence", "lnk_hk")
        ))
        .bind(now)
        .bind(&s2.se_lnk_hk)
        .bind(&s2.se_hash_diff)
        .bind(&src)
        .bind(&s2.se_sub_seq)
        .bind(&s2.se_char_start)
        .bind(&s2.se_char_end)
        .bind(&s2.se_bbox)
        .execute(&mut *tx)
        .await?;

        // Final run state.
        let run_hk = HashKey::run(&run.run_id);
        sqlx::query("INSERT INTO vault.hub_run (run_hk, run_id, load_dts, record_source) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(run_hk.as_str())
            .bind(run.run_id.0)
            .bind(now)
            .bind(&src)
            .execute(&mut *tx)
            .await?;
        insert_sat_run(&mut tx, &run_hk, now, &src, &run.dag_list_version, spec, &enum_str(&final_state), Some(run.started_at), Some(now)).await?;

        tx.commit().await?;
        Ok(())
    }
}

/// Document-local key for an entity the corpus resolver did not canonicalise.
pub fn local_entity_key(document_hk: &HashKey, local_id: &str) -> String {
    format!("local:{document_hk}:{local_id}")
}

/// Column arrays for the `UNNEST` inserts of `write_stage2`.
#[derive(Default)]
struct Stage2Columns {
    link_hk: Vec<String>,
    link_from: Vec<String>,
    link_to: Vec<String>,
    link_kind: Vec<String>,
    link_confidence: Vec<f32>,
    link_hash_diff: Vec<String>,

    entity_hk: Vec<String>,
    entity_canonical_key: Vec<String>,
    entity_name: Vec<String>,
    entity_type_id: Vec<Option<String>>,
    entity_raw_type: Vec<String>,
    entity_aliases: Vec<Value>,
    entity_review_status: Vec<String>,
    entity_hash_diff: Vec<String>,

    mention_lnk_hk: Vec<String>,
    mention_block_hk: Vec<String>,
    mention_entity_hk: Vec<String>,
    sm_lnk_hk: Vec<String>,
    sm_hash_diff: Vec<String>,
    sm_sub_seq: Vec<i32>,
    sm_char_start: Vec<i32>,
    sm_char_end: Vec<i32>,
    sm_surface: Vec<String>,
    sm_confidence: Vec<f32>,

    assertion_hk: Vec<String>,
    assertion_predicate: Vec<String>,
    assertion_arg_keys: Vec<String>,
    assertion_hash_diff: Vec<String>,
    assertion_record_source: Vec<String>,
    assertion_predicate_id: Vec<Option<String>>,
    assertion_kind: Vec<String>,
    assertion_qualifiers: Vec<Value>,
    assertion_literals: Vec<Value>,
    assertion_confidence: Vec<f32>,
    assertion_verified: Vec<bool>,
    assertion_provenance: Vec<Value>,

    arg_lnk_hk: Vec<String>,
    arg_assertion_hk: Vec<String>,
    arg_entity_hk: Vec<String>,
    arg_role: Vec<String>,

    ev_lnk_hk: Vec<String>,
    ev_assertion_hk: Vec<String>,
    ev_block_hk: Vec<Option<String>>,
    ev_page_hk: Vec<Option<String>>,
    se_lnk_hk: Vec<String>,
    se_hash_diff: Vec<String>,
    se_sub_seq: Vec<i32>,
    se_char_start: Vec<Option<i32>>,
    se_char_end: Vec<Option<i32>>,
    se_bbox: Vec<Option<Value>>,
}

impl Stage2Columns {
    fn of(spec: &JobSpec, doc: &StitchedDocument, ext: &ExtractionOutput, src: &str) -> Self {
        let mut c = Stage2Columns::default();
        let tenant = &spec.tenant;

        // Stitch links (deduplicated by key; the same pair may be linked twice with the same kind).
        let mut seen = BTreeSet::new();
        for l in &doc.links {
            let kind = enum_str(&l.kind);
            let hk = HashKey::link(&[&l.from, &l.to], Some(&kind)).to_string();
            if !seen.insert(hk.clone()) {
                continue;
            }
            c.link_hash_diff.push(hash_diff(&json!([doc.run_id.0, l.confidence])));
            c.link_hk.push(hk);
            c.link_from.push(l.from.to_string());
            c.link_to.push(l.to.to_string());
            c.link_kind.push(kind);
            c.link_confidence.push(l.confidence);
        }

        // Entities: canonical key if resolved, else a document-local key.
        let mut entity_key: HashMap<&str, HashKey> = HashMap::new();
        for e in &ext.entities {
            let local_key = local_entity_key(&doc.document_hk, &e.local_id);
            let (hk, canonical_key) = match &e.entity_hk {
                Some(hk) => (hk.clone(), e.name.trim().to_lowercase()),
                None => (HashKey::of(&[tenant.as_str(), doc.document_hk.as_str(), &e.local_id]), local_key),
            };
            entity_key.insert(e.local_id.as_str(), hk.clone());
            if c.entity_hk.contains(&hk.to_string()) {
                continue; // two local entities resolved to the same canonical one
            }
            let aliases = serde_json::to_value(&e.aliases).unwrap_or(Value::Array(vec![]));
            let review = if e.type_id.is_some() { "approved" } else { "pending" };
            c.entity_hash_diff.push(hash_diff(&json!([e.name, e.type_id, e.raw_type, aliases, review])));
            c.entity_hk.push(hk.to_string());
            c.entity_canonical_key.push(canonical_key);
            c.entity_name.push(e.name.clone());
            c.entity_type_id.push(e.type_id.clone());
            c.entity_raw_type.push(e.raw_type.clone());
            c.entity_aliases.push(aliases);
            c.entity_review_status.push(review.to_string());
        }

        // Mentions grouped per (block, entity) link; the group shares one hash_diff so the
        // multi-active satellite is re-inserted as a whole only when any mention changed.
        let mention_entity: HashMap<&str, &str> =
            ext.entities.iter().flat_map(|e| e.mentions.iter().map(move |m| (m.as_str(), e.local_id.as_str()))).collect();
        let mut groups: BTreeMap<(String, String), Vec<&vs_extract::Mention>> = BTreeMap::new();
        for m in &ext.mentions {
            let Some(hk) = mention_entity.get(m.id.as_str()).and_then(|l| entity_key.get(l)) else {
                continue;
            };
            groups.entry((m.block_hk.to_string(), hk.to_string())).or_default().push(m);
        }
        for ((block_hk, entity_hk), mut ms) in groups {
            ms.sort_by_key(|m| (m.char_start, m.char_end, m.surface.clone()));
            let lnk = HashKey::of(&[&block_hk, &entity_hk]).to_string();
            let hd = hash_diff(&json!(ms.iter().map(|m| json!([m.char_start, m.char_end, m.surface, m.confidence])).collect::<Vec<_>>()));
            c.mention_lnk_hk.push(lnk.clone());
            c.mention_block_hk.push(block_hk);
            c.mention_entity_hk.push(entity_hk);
            for (i, m) in ms.iter().enumerate() {
                c.sm_lnk_hk.push(lnk.clone());
                c.sm_hash_diff.push(hd.clone());
                c.sm_sub_seq.push(i as i32);
                c.sm_char_start.push(m.char_start as i32);
                c.sm_char_end.push(m.char_end as i32);
                c.sm_surface.push(m.surface.clone());
                c.sm_confidence.push(m.confidence);
            }
        }

        // Assertions. Literal arguments take part in the key so two attribute assertions
        // with the same predicate and different values stay distinct.
        for a in &ext.assertions {
            let mut arg_hks: Vec<HashKey> = Vec::new();
            let mut entity_args: Vec<(String, HashKey)> = Vec::new();
            let mut literals = Vec::new();
            for arg in &a.args {
                match &arg.value {
                    ArgValue::Entity { local_id } => {
                        if let Some(hk) = entity_key.get(local_id.as_str()) {
                            arg_hks.push(hk.clone());
                            entity_args.push((arg.role.clone(), hk.clone()));
                        }
                    }
                    ArgValue::Literal { value, unit } => {
                        arg_hks.push(HashKey::of(&[&arg.role, value, unit.as_deref().unwrap_or("")]));
                        literals.push(json!({ "role": arg.role, "value": value, "unit": unit }));
                    }
                }
            }
            let refs: Vec<&HashKey> = arg_hks.iter().collect();
            let hk = HashKey::assertion(&doc.run_id, &a.predicate, &refs);
            if c.assertion_hk.iter().any(|k| k == hk.as_str()) {
                continue;
            }
            let mut sorted: Vec<&str> = refs.iter().map(|k| k.as_str()).collect();
            sorted.sort_unstable();
            let prov = ext.provenance.get(a.provenance_idx);
            let model = prov.and_then(|p| p.model_ref.as_deref());
            let provenance = prov.map(|p| serde_json::to_value(p).unwrap_or(Value::Null)).unwrap_or(Value::Null);
            let qualifiers = serde_json::to_value(&a.qualifiers).unwrap_or(Value::Null);
            let literals = Value::Array(literals);
            let kind = enum_str(&a.kind);
            c.assertion_hash_diff.push(hash_diff(&json!([
                a.predicate,
                a.predicate_id,
                kind,
                qualifiers,
                literals,
                a.confidence,
                a.verified,
                provenance
            ])));
            c.assertion_record_source.push(if model.is_some() { record_source("doc", STAGE2_REF, model) } else { src.to_string() });
            c.assertion_predicate.push(a.predicate.clone());
            c.assertion_arg_keys.push(sorted.join("|"));
            c.assertion_predicate_id.push(a.predicate_id.clone());
            c.assertion_kind.push(kind);
            c.assertion_qualifiers.push(qualifiers);
            c.assertion_literals.push(literals);
            c.assertion_confidence.push(a.confidence);
            c.assertion_verified.push(a.verified);
            c.assertion_provenance.push(provenance);

            for (role, ehk) in &entity_args {
                let lnk = HashKey::link(&[&hk, ehk], Some(role)).to_string();
                if c.arg_lnk_hk.contains(&lnk) {
                    continue;
                }
                c.arg_lnk_hk.push(lnk);
                c.arg_assertion_hk.push(hk.to_string());
                c.arg_entity_hk.push(ehk.to_string());
                c.arg_role.push(role.clone());
            }

            // Evidence grouped per target (block or page).
            let mut ev: BTreeMap<(Option<String>, Option<String>), Vec<&Evidence>> = BTreeMap::new();
            for e in &a.evidence {
                let key = match e {
                    Evidence::Span { block_hk, .. } => (Some(block_hk.to_string()), None),
                    Evidence::Region { page_hk, .. } => (None, Some(page_hk.to_string())),
                };
                ev.entry(key).or_default().push(e);
            }
            for ((block_hk, page_hk), es) in ev {
                let target = block_hk.clone().or_else(|| page_hk.clone()).unwrap_or_default();
                let lnk = HashKey::of(&[hk.as_str(), &target]).to_string();
                let rows: Vec<(Option<i32>, Option<i32>, Option<Value>)> = es
                    .iter()
                    .map(|e| match e {
                        Evidence::Span { char_start, char_end, .. } => (Some(*char_start as i32), Some(*char_end as i32), None),
                        Evidence::Region { bbox, .. } => (None, None, Some(serde_json::to_value(bbox).unwrap_or(Value::Null))),
                    })
                    .collect();
                let hd = hash_diff(&json!(rows.iter().map(|(s, e, b)| json!([s, e, b])).collect::<Vec<_>>()));
                c.ev_lnk_hk.push(lnk.clone());
                c.ev_assertion_hk.push(hk.to_string());
                c.ev_block_hk.push(block_hk);
                c.ev_page_hk.push(page_hk);
                for (i, (s, e, b)) in rows.into_iter().enumerate() {
                    c.se_lnk_hk.push(lnk.clone());
                    c.se_hash_diff.push(hd.clone());
                    c.se_sub_seq.push(i as i32);
                    c.se_char_start.push(s);
                    c.se_char_end.push(e);
                    c.se_bbox.push(b);
                }
            }
        }
        c
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
