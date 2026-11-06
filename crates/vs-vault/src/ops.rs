//! Operational state (`ops` schema): jobs and page tasks. Mutable, not part of the vault.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use vs_core::{HashKey, JobSpec, JobState, RunId, TenantId};

use crate::{enum_str, VaultError};

#[derive(Debug, Clone)]
pub struct JobRow {
    pub job_id: Uuid,
    pub batch_id: Option<Uuid>,
    pub tenant_id: String,
    pub document_hk: Option<String>,
    pub run_id: RunId,
    pub state: String,
    pub spec: JobSpec,
    pub pages_total: Option<i32>,
    pub pages_done: i32,
    pub pages_failed: i32,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Page counters after a page finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageProgress {
    pub done: i32,
    pub failed: i32,
    pub total: i32,
    /// This call moved the page to a final state (false on a repeated call for the
    /// same page, e.g. after a redelivery).
    pub counted: bool,
}

impl PageProgress {
    /// True for exactly one call per job: the one that finished the last page.
    pub fn completed_job(&self) -> bool {
        self.counted && self.done + self.failed == self.total
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageOutcome {
    Done,
    Failed,
}

pub struct Ops {
    pub pool: PgPool,
}

impl Ops {
    pub async fn create_job(&self, job_id: Uuid, batch_id: Option<Uuid>, run_id: RunId, spec: &JobSpec) -> Result<(), VaultError> {
        sqlx::query(
            "INSERT INTO ops.jobs (job_id, batch_id, tenant_id, run_id, state, priority, spec)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(job_id)
        .bind(batch_id)
        .bind(spec.tenant.as_str())
        .bind(run_id.0)
        .bind(enum_str(&JobState::Received))
        .bind(enum_str(&spec.priority))
        .bind(serde_json::to_value(spec).unwrap_or_default())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Tenant-scoped: another tenant's job is reported as missing.
    pub async fn get_job(&self, tenant: &TenantId, job_id: Uuid) -> Result<Option<JobRow>, VaultError> {
        let row = sqlx::query(
            "SELECT job_id, batch_id, tenant_id, document_hk, run_id, state, spec, pages_total, pages_done, pages_failed,
                    error, created_at, updated_at
             FROM ops.jobs WHERE job_id = $1 AND tenant_id = $2",
        )
        .bind(job_id)
        .bind(tenant.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| -> Result<JobRow, VaultError> {
            let spec: serde_json::Value = r.try_get("spec")?;
            Ok(JobRow {
                job_id: r.try_get("job_id")?,
                batch_id: r.try_get("batch_id")?,
                tenant_id: r.try_get("tenant_id")?,
                document_hk: r.try_get::<Option<String>, _>("document_hk")?.map(|s| s.trim_end().to_string()),
                run_id: RunId(r.try_get("run_id")?),
                state: r.try_get("state")?,
                spec: serde_json::from_value(spec).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                pages_total: r.try_get("pages_total")?,
                pages_done: r.try_get("pages_done")?,
                pages_failed: r.try_get("pages_failed")?,
                error: r.try_get("error")?,
                created_at: r.try_get("created_at")?,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .transpose()
    }

    pub async fn set_state(&self, job_id: Uuid, state: JobState, error: Option<&str>) -> Result<(), VaultError> {
        sqlx::query("UPDATE ops.jobs SET state = $2, error = COALESCE($3, error), updated_at = now() WHERE job_id = $1")
            .bind(job_id)
            .bind(enum_str(&state))
            .bind(error)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// End of ingest: records the document and page count and creates the page tasks.
    /// Idempotent (ingest retries).
    pub async fn start_pages(&self, job_id: Uuid, document_hk: &HashKey, pages_total: u32) -> Result<(), VaultError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE ops.jobs SET document_hk = $2, pages_total = $3, state = $4, updated_at = now() WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(document_hk.as_str())
        .bind(pages_total as i32)
        .bind(enum_str(&JobState::PagesProcessing))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ops.page_tasks (job_id, page_no, state)
             SELECT $1, n, 'pending' FROM generate_series(1, $2) AS n
             ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(pages_total as i32)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A page task was picked up (attempt counter, for status reporting).
    pub async fn page_started(&self, job_id: Uuid, page_no: u32) -> Result<(), VaultError> {
        sqlx::query(
            "UPDATE ops.page_tasks SET state = 'running', attempts = attempts + 1, updated_at = now()
             WHERE job_id = $1 AND page_no = $2 AND state IN ('pending', 'running')",
        )
        .bind(job_id)
        .bind(page_no as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Moves a page to its final state and bumps the job counter in one statement.
    /// Only the first call per page counts, so redeliveries cannot double count and
    /// exactly one caller sees `completed_job()`.
    pub async fn page_finished(
        &self,
        job_id: Uuid,
        page_no: u32,
        outcome: PageOutcome,
        dag_ref: Option<&str>,
        rule_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<PageProgress, VaultError> {
        let state = match outcome {
            PageOutcome::Done => "done",
            PageOutcome::Failed => "failed",
        };
        let row = sqlx::query(
            "WITH t AS (
                 UPDATE ops.page_tasks SET state = $3, dag_ref = $4, rule_id = $5, error = $6, updated_at = now()
                 WHERE job_id = $1 AND page_no = $2 AND state NOT IN ('done', 'failed')
                 RETURNING 1
             ), n AS (SELECT count(*)::int AS c FROM t)
             UPDATE ops.jobs SET
                 pages_done   = pages_done   + CASE WHEN $3 = 'done'   THEN (SELECT c FROM n) ELSE 0 END,
                 pages_failed = pages_failed + CASE WHEN $3 = 'failed' THEN (SELECT c FROM n) ELSE 0 END,
                 updated_at = now()
             WHERE job_id = $1
             RETURNING pages_done, pages_failed, COALESCE(pages_total, 0) AS pages_total, (SELECT c FROM n) AS counted",
        )
        .bind(job_id)
        .bind(page_no as i32)
        .bind(state)
        .bind(dag_ref)
        .bind(rule_id)
        .bind(error)
        .fetch_one(&self.pool)
        .await?;
        Ok(PageProgress {
            done: row.try_get("pages_done")?,
            failed: row.try_get("pages_failed")?,
            total: row.try_get("pages_total")?,
            counted: row.try_get::<i32, _>("counted")? == 1,
        })
    }
}
