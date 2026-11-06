//! Operational state (`ops` and `registry` schemas): jobs, page tasks, batches, DAG
//! lists, idempotency keys, ontology. Mutable, not part of the vault. Also the read
//! side of the vault for the API (`document_summary`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use vs_core::{HashKey, JobSpec, JobState, RunId, TenantId};
use vs_router::DagList;

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
        self.counted && self.all_finished()
    }

    /// Every page of the job is in a final state. Unlike `completed_job` this is also
    /// true on a repeated call for an already-final page (redelivery of a page task
    /// whose earlier attempt finished the page but died before it could publish the doc
    /// task), so the caller can re-publish; JetStream de-dup absorbs the duplicate.
    pub fn all_finished(&self) -> bool {
        self.total > 0 && self.done + self.failed == self.total
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageOutcome {
    Done,
    Failed,
}

/// Input of `Ops::create_jobs`.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub job_id: Uuid,
    pub run_id: RunId,
    pub spec: JobSpec,
    pub document_hk: Option<HashKey>,
}

/// When a job entered each state, in order.
#[derive(Debug, Clone, Serialize)]
pub struct JobTimings {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub states: Vec<StateAt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateAt {
    pub state: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct BatchPage {
    pub batch_id: Uuid,
    pub tenant_id: String,
    pub created_at: DateTime<Utc>,
    /// Documents announced at creation.
    pub documents: i32,
    /// Jobs per state (`JobState` serde names).
    pub counts: BTreeMap<String, i64>,
    pub total_jobs: i64,
    pub page: u32,
    pub page_size: u32,
    pub jobs: Vec<JobRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DagListRow {
    pub version: String,
    pub tenant_id: Option<String>,
    pub status: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub activated_at: Option<DateTime<Utc>>,
}

/// The newest job of a tenant for a document.
#[derive(Debug, Clone)]
pub struct CurrentRun {
    pub job_id: Uuid,
    pub run_id: RunId,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageSummary {
    pub page_no: i32,
    pub profile: Value,
    pub dag_ref: String,
    pub rule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocumentSummary {
    pub document_hk: String,
    pub run_id: Uuid,
    pub run_state: Option<String>,
    pub dag_list_version: Option<String>,
    pub page_count: Option<i32>,
    pub pages: Vec<PageSummary>,
    pub blocks_by_kind: BTreeMap<String, i64>,
    pub entities: i64,
    pub assertions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityTypeRow {
    pub type_id: String,
    pub label: String,
    pub description: Option<String>,
    pub synonyms: Vec<String>,
    pub parent_type_id: Option<String>,
    pub review_status: String,
    pub merged_into: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationTypeRow {
    pub predicate_id: String,
    pub label: String,
    pub roles: Vec<String>,
    pub synonyms: Vec<String>,
    pub review_status: String,
    pub merged_into: Option<String>,
}

pub struct Ops {
    pub pool: PgPool,
}

fn job_row(r: sqlx::postgres::PgRow) -> Result<JobRow, VaultError> {
    let spec: Value = r.try_get("spec")?;
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
}

const JOB_COLUMNS: &str = "job_id, batch_id, tenant_id, document_hk, run_id, state, spec, pages_total, pages_done, pages_failed, \
                           error, created_at, updated_at";

impl Ops {
    pub async fn create_job(&self, job_id: Uuid, batch_id: Option<Uuid>, run_id: RunId, spec: &JobSpec) -> Result<(), VaultError> {
        self.create_job_for_document(job_id, batch_id, run_id, spec, None).await
    }

    /// `create_job` with the document hash key already known (multipart upload: the API
    /// hashed the file). Ingest sets it otherwise.
    pub async fn create_job_for_document(
        &self,
        job_id: Uuid,
        batch_id: Option<Uuid>,
        run_id: RunId,
        spec: &JobSpec,
        document_hk: Option<&HashKey>,
    ) -> Result<(), VaultError> {
        self.create_jobs(batch_id, &[NewJob { job_id, run_id, spec: spec.clone(), document_hk: document_hk.cloned() }]).await
    }

    /// Bulk variant for batches: one statement for the jobs, one for the state log.
    pub async fn create_jobs(&self, batch_id: Option<Uuid>, jobs: &[NewJob]) -> Result<(), VaultError> {
        if jobs.is_empty() {
            return Ok(());
        }
        let job_ids: Vec<Uuid> = jobs.iter().map(|j| j.job_id).collect();
        let tenants: Vec<&str> = jobs.iter().map(|j| j.spec.tenant.as_str()).collect();
        let doc_hks: Vec<Option<String>> = jobs.iter().map(|j| j.document_hk.as_ref().map(|h| h.to_string())).collect();
        let run_ids: Vec<Uuid> = jobs.iter().map(|j| j.run_id.0).collect();
        let priorities: Vec<String> = jobs.iter().map(|j| enum_str(&j.spec.priority)).collect();
        let specs: Vec<Value> = jobs.iter().map(|j| serde_json::to_value(&j.spec).unwrap_or_default()).collect();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO ops.jobs (job_id, batch_id, tenant_id, document_hk, run_id, state, priority, spec)
             SELECT v.job_id, $7, v.tenant_id, v.document_hk, v.run_id, $8, v.priority, v.spec
             FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::uuid[], $5::text[], $6::jsonb[])
               AS v(job_id, tenant_id, document_hk, run_id, priority, spec)",
        )
        .bind(&job_ids)
        .bind(&tenants)
        .bind(&doc_hks)
        .bind(&run_ids)
        .bind(&priorities)
        .bind(&specs)
        .bind(batch_id)
        .bind(enum_str(&JobState::Received))
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO ops.job_state_log (job_id, state) SELECT j, $2 FROM UNNEST($1::uuid[]) AS j")
            .bind(&job_ids)
            .bind(enum_str(&JobState::Received))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Tenant-scoped: another tenant's job is reported as missing.
    pub async fn get_job(&self, tenant: &TenantId, job_id: Uuid) -> Result<Option<JobRow>, VaultError> {
        let row = sqlx::query(&format!("SELECT {JOB_COLUMNS} FROM ops.jobs WHERE job_id = $1 AND tenant_id = $2"))
            .bind(job_id)
            .bind(tenant.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.map(job_row).transpose()
    }

    /// Creation, last update and the state history of a job (tenant-scoped).
    pub async fn job_timings(&self, tenant: &TenantId, job_id: Uuid) -> Result<Option<JobTimings>, VaultError> {
        let Some(head) = sqlx::query("SELECT created_at, updated_at FROM ops.jobs WHERE job_id = $1 AND tenant_id = $2")
            .bind(job_id)
            .bind(tenant.as_str())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let states = sqlx::query("SELECT state, at FROM ops.job_state_log WHERE job_id = $1 ORDER BY at")
            .bind(job_id)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| Ok(StateAt { state: r.try_get("state")?, at: r.try_get("at")? }))
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        Ok(Some(JobTimings { created_at: head.try_get("created_at")?, updated_at: head.try_get("updated_at")?, states }))
    }

    // ───────────── batches ─────────────

    pub async fn create_batch(&self, batch_id: Uuid, tenant: &TenantId, documents: u32) -> Result<(), VaultError> {
        sqlx::query("INSERT INTO ops.batches (batch_id, tenant_id, documents) VALUES ($1, $2, $3)")
            .bind(batch_id)
            .bind(tenant.as_str())
            .bind(documents as i32)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Counts per state and one page of jobs (`page` is 0-based). Tenant-scoped.
    pub async fn get_batch(&self, tenant: &TenantId, batch_id: Uuid, page: u32, page_size: u32) -> Result<Option<BatchPage>, VaultError> {
        let Some(head) = sqlx::query("SELECT created_at, documents FROM ops.batches WHERE batch_id = $1 AND tenant_id = $2")
            .bind(batch_id)
            .bind(tenant.as_str())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let mut counts = BTreeMap::new();
        let mut total_jobs = 0i64;
        for r in
            sqlx::query("SELECT state, count(*) AS n FROM ops.jobs WHERE batch_id = $1 GROUP BY state").bind(batch_id).fetch_all(&self.pool).await?
        {
            let n: i64 = r.try_get("n")?;
            total_jobs += n;
            counts.insert(r.try_get::<String, _>("state")?, n);
        }
        let page_size = page_size.clamp(1, 1000);
        let jobs = sqlx::query(&format!("SELECT {JOB_COLUMNS} FROM ops.jobs WHERE batch_id = $1 ORDER BY created_at, job_id LIMIT $2 OFFSET $3"))
            .bind(batch_id)
            .bind(page_size as i64)
            .bind(page as i64 * page_size as i64)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(job_row)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(BatchPage {
            batch_id,
            tenant_id: tenant.as_str().to_string(),
            created_at: head.try_get("created_at")?,
            documents: head.try_get("documents")?,
            counts,
            total_jobs,
            page,
            page_size,
            jobs,
        }))
    }

    // ───────────── documents ─────────────

    /// The tenant's newest job for a document, if any (`skip_if_processed`).
    pub async fn current_run(&self, tenant: &TenantId, document_hk: &HashKey) -> Result<Option<CurrentRun>, VaultError> {
        let row = sqlx::query(
            "SELECT job_id, run_id, state FROM ops.jobs WHERE tenant_id = $1 AND document_hk = $2
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok(CurrentRun { job_id: r.try_get("job_id")?, run_id: RunId(r.try_get("run_id")?), state: r.try_get("state")? })).transpose()
    }

    /// Resolves the run to read for a document (`run_id` or the vault's current run) and
    /// checks that the document belongs to the tenant. `None` if either fails.
    pub async fn document_run(&self, tenant: &TenantId, document_hk: &HashKey, run_id: Option<RunId>) -> Result<Option<RunId>, VaultError> {
        let owned: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM vault.hub_document WHERE document_hk = $1 AND tenant_id = $2)")
            .bind(document_hk.as_str())
            .bind(tenant.as_str())
            .fetch_one(&self.pool)
            .await?;
        if !owned {
            return Ok(None);
        }
        let run: Option<Uuid> = match run_id {
            Some(r) => {
                sqlx::query_scalar(
                    "SELECT r.run_id FROM vault.lnk_run_document l JOIN vault.hub_run r ON r.run_hk = l.run_hk
                 WHERE l.document_hk = $1 AND r.run_id = $2",
                )
                .bind(document_hk.as_str())
                .bind(r.0)
                .fetch_optional(&self.pool)
                .await?
            }
            None => {
                sqlx::query_scalar("SELECT run_id FROM vault.v_current_run WHERE document_hk = $1")
                    .bind(document_hk.as_str())
                    .fetch_optional(&self.pool)
                    .await?
            }
        };
        Ok(run.map(RunId))
    }

    /// Run summary read from the vault: pages with profile and DAG, block counts by kind,
    /// entity and assertion counts. Tenant-checked through `hub_document`.
    pub async fn document_summary(
        &self,
        tenant: &TenantId,
        document_hk: &HashKey,
        run_id: Option<RunId>,
    ) -> Result<Option<DocumentSummary>, VaultError> {
        let Some(run) = self.document_run(tenant, document_hk, run_id).await? else {
            return Ok(None);
        };
        let run_row = sqlx::query(
            "SELECT s.state, s.dag_list_version FROM vault.sat_run s JOIN vault.hub_run r ON r.run_hk = s.run_hk
             WHERE r.run_id = $1 ORDER BY s.load_dts DESC LIMIT 1",
        )
        .bind(run.0)
        .fetch_optional(&self.pool)
        .await?;
        let (run_state, dag_list_version) = match run_row {
            Some(r) => (r.try_get("state")?, r.try_get("dag_list_version")?),
            None => (None, None),
        };
        let page_count: Option<i32> =
            sqlx::query_scalar("SELECT page_count FROM vault.sat_document_meta WHERE document_hk = $1 ORDER BY load_dts DESC LIMIT 1")
                .bind(document_hk.as_str())
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let pages = sqlx::query(
            "SELECT DISTINCT ON (p.page_no) p.page_no, s.profile, s.dag_ref, s.rule_id
             FROM vault.sat_page_profile s JOIN vault.hub_page p ON p.page_hk = s.page_hk
             WHERE p.document_hk = $1 AND s.run_id = $2
             ORDER BY p.page_no, s.load_dts DESC",
        )
        .bind(document_hk.as_str())
        .bind(run.0)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| {
            Ok(PageSummary {
                page_no: r.try_get("page_no")?,
                profile: r.try_get("profile")?,
                dag_ref: r.try_get("dag_ref")?,
                rule_id: r.try_get("rule_id")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let mut blocks_by_kind = BTreeMap::new();
        for r in sqlx::query(
            "SELECT c.kind, count(*) AS n
             FROM (SELECT DISTINCT ON (block_hk) block_hk, kind FROM vault.sat_block_content ORDER BY block_hk, load_dts DESC) c
             JOIN vault.hub_block b ON b.block_hk = c.block_hk
             JOIN vault.hub_page p ON p.page_hk = b.page_hk
             WHERE p.document_hk = $1 AND b.run_id = $2 GROUP BY c.kind",
        )
        .bind(document_hk.as_str())
        .bind(run.0)
        .fetch_all(&self.pool)
        .await?
        {
            blocks_by_kind.insert(r.try_get::<String, _>("kind")?, r.try_get::<i64, _>("n")?);
        }
        let entities: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT m.entity_hk) FROM vault.lnk_mention m
             JOIN vault.hub_block b ON b.block_hk = m.block_hk
             JOIN vault.hub_page p ON p.page_hk = b.page_hk
             WHERE p.document_hk = $1 AND b.run_id = $2",
        )
        .bind(document_hk.as_str())
        .bind(run.0)
        .fetch_one(&self.pool)
        .await?;
        let assertions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM vault.hub_assertion WHERE run_id = $1").bind(run.0).fetch_one(&self.pool).await?;
        Ok(Some(DocumentSummary {
            document_hk: document_hk.to_string(),
            run_id: run.0,
            run_state,
            dag_list_version,
            page_count,
            pages,
            blocks_by_kind,
            entities,
            assertions,
        }))
    }

    // ───────────── idempotency ─────────────

    /// Stored response for an `Idempotency-Key`, if younger than 24 h.
    pub async fn get_idempotent(&self, tenant: &TenantId, key: &str) -> Result<Option<Value>, VaultError> {
        Ok(sqlx::query_scalar(
            "SELECT response FROM ops.idempotency_keys
             WHERE tenant_id = $1 AND key = $2 AND created_at > now() - interval '24 hours'",
        )
        .bind(tenant.as_str())
        .bind(key)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Stores (or refreshes an expired) response; a live entry is left as is.
    pub async fn put_idempotent(&self, tenant: &TenantId, key: &str, response: &Value) -> Result<(), VaultError> {
        sqlx::query(
            "INSERT INTO ops.idempotency_keys (tenant_id, key, response) VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, key) DO UPDATE SET response = EXCLUDED.response, created_at = now()
             WHERE ops.idempotency_keys.created_at <= now() - interval '24 hours'",
        )
        .bind(tenant.as_str())
        .bind(key)
        .bind(response)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes expired keys; returns how many. Call occasionally (e.g. from the API on a timer).
    pub async fn purge_idempotency_keys(&self) -> Result<u64, VaultError> {
        Ok(sqlx::query("DELETE FROM ops.idempotency_keys WHERE created_at <= now() - interval '24 hours'").execute(&self.pool).await?.rows_affected())
    }

    // ───────────── DAG lists ─────────────

    /// The tenant's lists and the global ones, newest first.
    pub async fn dag_lists_list(&self, tenant: &TenantId) -> Result<Vec<DagListRow>, VaultError> {
        sqlx::query(
            "SELECT version, tenant_id, status, created_by, created_at, activated_at FROM registry.dag_lists
             WHERE tenant_id = $1 OR tenant_id IS NULL ORDER BY created_at DESC",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| {
            Ok(DagListRow {
                version: r.try_get("version")?,
                tenant_id: r.try_get("tenant_id")?,
                status: r.try_get("status")?,
                created_by: r.try_get("created_by")?,
                created_at: r.try_get("created_at")?,
                activated_at: r.try_get("activated_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
    }

    /// Stores a validated list as `candidate`. `tenant = None` = global. Returns false if
    /// the version exists already.
    pub async fn dag_list_insert_candidate(
        &self,
        version: &str,
        tenant: Option<&TenantId>,
        body: &DagList,
        created_by: Option<&str>,
    ) -> Result<bool, VaultError> {
        let n = sqlx::query(
            "INSERT INTO registry.dag_lists (version, tenant_id, status, body, created_by) VALUES ($1, $2, 'candidate', $3, $4)
             ON CONFLICT (version) DO NOTHING",
        )
        .bind(version)
        .bind(tenant.map(|t| t.as_str()))
        .bind(serde_json::to_value(body).unwrap_or_default())
        .bind(created_by)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Makes `version` the active list of its scope (its own tenant, or global) and
    /// retires the previously active one in the same transaction. `tenant` must match the
    /// list's scope. Returns false if there is no such list in that scope.
    pub async fn dag_list_activate(&self, version: &str, tenant: Option<&TenantId>) -> Result<bool, VaultError> {
        let mut tx = self.pool.begin().await?;
        let scope = tenant.map(|t| t.as_str()).unwrap_or("");
        // Serialise activations per scope.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('dag_lists:' || $1))").bind(scope).execute(&mut *tx).await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM registry.dag_lists WHERE version = $1 AND COALESCE(tenant_id, '') = $2)")
            .bind(version)
            .bind(scope)
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            return Ok(false);
        }
        sqlx::query("UPDATE registry.dag_lists SET status = 'retired' WHERE status = 'active' AND COALESCE(tenant_id, '') = $2 AND version <> $1")
            .bind(version)
            .bind(scope)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE registry.dag_lists SET status = 'active', activated_at = COALESCE(activated_at, now()) WHERE version = $1")
            .bind(version)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn dag_list_get(&self, version: &str) -> Result<Option<DagList>, VaultError> {
        let body: Option<Value> =
            sqlx::query_scalar("SELECT body FROM registry.dag_lists WHERE version = $1").bind(version).fetch_optional(&self.pool).await?;
        body.map(|b| serde_json::from_value(b).map_err(|e| VaultError::Schema(format!("dag list {version}: {e}")))).transpose()
    }

    /// The tenant's active list, else the global one.
    pub async fn active_dag_list(&self, tenant: &TenantId) -> Result<Option<(String, DagList)>, VaultError> {
        let row = sqlx::query(
            "SELECT version, body FROM registry.dag_lists
             WHERE status = 'active' AND (tenant_id = $1 OR tenant_id IS NULL)
             ORDER BY tenant_id NULLS LAST LIMIT 1",
        )
        .bind(tenant.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let version: String = r.try_get("version")?;
            let body: Value = r.try_get("body")?;
            let list = serde_json::from_value(body).map_err(|e| VaultError::Schema(format!("dag list {version}: {e}")))?;
            Ok((version, list))
        })
        .transpose()
    }

    // ───────────── ontology (registry) ─────────────

    pub async fn entity_types(&self) -> Result<Vec<EntityTypeRow>, VaultError> {
        sqlx::query(
            "SELECT type_id, label, description, synonyms, parent_type_id, review_status, merged_into FROM registry.entity_types ORDER BY type_id",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| {
            Ok(EntityTypeRow {
                type_id: r.try_get("type_id")?,
                label: r.try_get("label")?,
                description: r.try_get("description")?,
                synonyms: r.try_get("synonyms")?,
                parent_type_id: r.try_get("parent_type_id")?,
                review_status: r.try_get("review_status")?,
                merged_into: r.try_get("merged_into")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
    }

    pub async fn relation_types(&self) -> Result<Vec<RelationTypeRow>, VaultError> {
        sqlx::query("SELECT predicate_id, label, roles, synonyms, review_status, merged_into FROM registry.relation_types ORDER BY predicate_id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| {
                let roles: Value = r.try_get("roles")?;
                Ok(RelationTypeRow {
                    predicate_id: r.try_get("predicate_id")?,
                    label: r.try_get("label")?,
                    roles: serde_json::from_value(roles).unwrap_or_default(),
                    synonyms: r.try_get("synonyms")?,
                    review_status: r.try_get("review_status")?,
                    merged_into: r.try_get("merged_into")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(Into::into)
    }

    /// New type found by normalisation; `review_status = pending` (A10). Idempotent.
    pub async fn insert_pending_entity_type(&self, type_id: &str, label: &str) -> Result<(), VaultError> {
        sqlx::query("INSERT INTO registry.entity_types (type_id, label) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(type_id)
            .bind(label)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_pending_relation_type(&self, predicate_id: &str, label: &str, roles: &[String]) -> Result<(), VaultError> {
        sqlx::query("INSERT INTO registry.relation_types (predicate_id, label, roles) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(predicate_id)
            .bind(label)
            .bind(serde_json::to_value(roles).unwrap_or_default())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Version of the active DAG list for the tenant, else the global one.
    pub async fn active_dag_list_version(&self, tenant: &TenantId) -> Result<Option<String>, VaultError> {
        Ok(sqlx::query_scalar(
            "SELECT version FROM registry.dag_lists
             WHERE status = 'active' AND (tenant_id = $1 OR tenant_id IS NULL)
             ORDER BY tenant_id NULLS LAST LIMIT 1",
        )
        .bind(tenant.as_str())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn set_state(&self, job_id: Uuid, state: JobState, error: Option<&str>) -> Result<(), VaultError> {
        sqlx::query(
            "WITH u AS (
                 UPDATE ops.jobs SET state = $2, error = COALESCE($3, error), updated_at = now()
                 WHERE job_id = $1 AND state <> $2 RETURNING job_id
             )
             INSERT INTO ops.job_state_log (job_id, state) SELECT job_id, $2 FROM u",
        )
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
            "WITH u AS (
                 UPDATE ops.jobs SET document_hk = $2, pages_total = $3, state = $4, updated_at = now()
                 WHERE job_id = $1 AND state <> $4 RETURNING job_id
             )
             INSERT INTO ops.job_state_log (job_id, state) SELECT job_id, $4 FROM u",
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

#[cfg(test)]
mod tests {
    use super::PageProgress;

    #[test]
    fn page_progress_completion() {
        let p = |done, failed, total, counted| PageProgress { done, failed, total, counted };
        assert!(!p(1, 0, 3, true).completed_job() && !p(1, 0, 3, true).all_finished());
        assert!(p(2, 1, 3, true).completed_job() && p(2, 1, 3, true).all_finished());
        // Redelivery of an already-final page: nothing counted, but the job is finished.
        assert!(!p(2, 1, 3, false).completed_job() && p(2, 1, 3, false).all_finished());
        assert!(!p(0, 0, 0, false).all_finished(), "a job without pages is never finished by a page");
    }
}
