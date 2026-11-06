//! Data Vault 2.0 writer. See `SPEC.md`.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::PgPool;

use vs_core::{HashKey, JobSpec, PageResult, RunId};
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

#[async_trait]
pub trait VaultWriter: Send + Sync {
    async fn write_document(&self, run: RunId, spec: &JobSpec, meta: &DocumentMeta, dag_list_version: &str) -> Result<(), VaultError>;
    async fn write_page(&self, page: &PageResult, rule_id: Option<&str>) -> Result<(), VaultError>;
    async fn write_stage2(&self, doc: &StitchedDocument, ext: &ExtractionOutput) -> Result<(), VaultError>;
}

pub struct PgVault {
    pub pool: PgPool,
}

pub fn record_source(component: &str, what: &str, model: Option<&str>) -> String {
    format!("vs/{component}/{what}/{}", model.unwrap_or("cpu"))
}

#[async_trait]
impl VaultWriter for PgVault {
    async fn write_document(&self, run: RunId, spec: &JobSpec, meta: &DocumentMeta, dag_list_version: &str) -> Result<(), VaultError> {
        let now = Utc::now();
        let src = record_source("ingest", dag_list_version, None);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO vault.hub_document (document_hk, tenant_id, content_sha256, load_dts, record_source)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(meta.document_hk.as_str())
        .bind(spec.tenant.as_str())
        .bind(&meta.content_sha256)
        .bind(now)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
        let _ = run;
        // Remaining tables (sat_document_meta, hub_run, sat_run, lnk_run_document, hub_page,
        // lnk_document_page): same pattern, batched with UNNEST. See SPEC.md "Write points".
        // The transaction is rolled back on drop until all tables are written.
        drop(tx);
        Err(VaultError::NotImplemented("write_document: remaining tables"))
    }

    async fn write_page(&self, _page: &PageResult, _rule_id: Option<&str>) -> Result<(), VaultError> {
        Err(VaultError::NotImplemented("write_page"))
    }

    async fn write_stage2(&self, _doc: &StitchedDocument, _ext: &ExtractionOutput) -> Result<(), VaultError> {
        Err(VaultError::NotImplemented("write_stage2"))
    }
}
