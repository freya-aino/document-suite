//! Read scoping inside an organization, and the two tables next to it: job leases and
//! token revocation.
//!
//! The organization is the isolation boundary and always was — it is key material in
//! every hash key, blob prefix and vector partition (D11), and nothing here loosens it.
//! What this module adds is the scoping *within* one: which member of an organization
//! may read which of its documents.
//!
//! **Where the rows live, and why not in the vault.** The vault is insert-only by
//! design (D5) and insert-only at the database level (migration 0003 grants it neither
//! UPDATE nor DELETE), so a share cannot be withdrawn there and an erasure request
//! cannot be served there. Access is mutable state, so it lives in `ops`, beside the
//! jobs — which is also the only other place a person's identifier is stored, so there
//! is one surface to erase rather than two (`docs/SPEC.md` §5.6).

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use ds_core::{HashKey, JobSpec, Owner, RunId, TenantId};

use crate::ops::Ops;
use crate::VaultError;

/// A `HashKey` from a hash key that came out of our own column.
fn hk_from(hex: &str) -> Result<HashKey, sqlx::Error> {
    crate::hash_key_from_hex(hex).ok_or_else(|| sqlx::Error::Decode(format!("{hex:?} is not a hash key").into()))
}

/// Seconds of a duration, for the `$n::bigint * interval '1 second'` form these queries
/// use. Durations are passed as seconds rather than as an interval because that needs no
/// agreement between sqlx's interval type and the column's.
fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// How wide a document is open inside its organization.
///
/// Ordered from narrowest to widest, which is also the order the checks in
/// [`Reader::may_read`] run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// The owner alone, plus whoever holds an explicit grant.
    Private,
    /// Everyone in the owner's department at upload time.
    Department,
    /// Everyone in any of the document's workspaces.
    Workspace,
    /// Every member of the organization — what every document used to be.
    Organization,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Department => "department",
            Visibility::Workspace => "workspace",
            Visibility::Organization => "organization",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "private" => Some(Visibility::Private),
            "department" => Some(Visibility::Department),
            "workspace" => Some(Visibility::Workspace),
            "organization" => Some(Visibility::Organization),
            _ => None,
        }
    }
}

/// Who a grant is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GranteeKind {
    /// One person, by Keycloak `sub`.
    Subject,
    Department,
    Workspace,
}

impl GranteeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GranteeKind::Subject => "subject",
            GranteeKind::Department => "department",
            GranteeKind::Workspace => "workspace",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "subject" => Some(GranteeKind::Subject),
            "department" => Some(GranteeKind::Department),
            "workspace" => Some(GranteeKind::Workspace),
            _ => None,
        }
    }
}

/// One share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    pub kind: GranteeKind,
    pub id: String,
    pub created_at: DateTime<Utc>,
}

/// A document's access row, with its grants.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentAccess {
    pub document_hk: String,
    /// `None` for a document whose job carried no owner.
    pub owner_subject: Option<String>,
    pub owner_department: Option<String>,
    pub workspaces: Vec<String>,
    pub visibility: Visibility,
    pub grants: Vec<Grant>,
    pub updated_at: DateTime<Utc>,
}

/// Who is asking. Built from the verified token, never from a request body.
///
/// `admin` is here rather than checked by the caller because "an administrator reads
/// everything" is part of the access rule and belongs next to the rest of it.
#[derive(Debug, Clone)]
pub struct Reader {
    pub subject: String,
    pub department: Option<String>,
    pub workspaces: Vec<String>,
    /// Holds the configured admin role *and* the deployment lets administrators read
    /// every document (`api.admin_reads_all`). Resolved by the caller, which is the only
    /// place that knows the configuration.
    pub admin: bool,
}

impl Reader {
    /// Whether this reader may read a document with this access row.
    ///
    /// `None` means no row exists, which is a document uploaded before anything recorded
    /// an owner: readable by the organization, as it has been all along. Inventing an
    /// owner for it would lock a tenant out of its own history.
    pub fn may_read(&self, access: Option<&DocumentAccess>) -> bool {
        if self.admin {
            return true;
        }
        let Some(a) = access else {
            return true;
        };
        // The owner always reads their own document, whatever its visibility.
        if a.owner_subject.as_deref() == Some(self.subject.as_str()) {
            return true;
        }
        // A document with no recorded owner is the organization's: `private` on such a
        // row would be readable by nobody at all, which is not a state anyone asked for.
        if a.owner_subject.is_none() {
            return true;
        }
        let by_visibility = match a.visibility {
            Visibility::Private => false,
            Visibility::Department => {
                // Two members of no department are not colleagues: `None == None` must
                // not open a document to everyone whose department claim is missing.
                matches!((&a.owner_department, &self.department), (Some(o), Some(r)) if o == r)
            }
            Visibility::Workspace => a.workspaces.iter().any(|w| self.workspaces.contains(w)),
            Visibility::Organization => true,
        };
        by_visibility || self.matches_a_grant(&a.grants)
    }

    fn matches_a_grant(&self, grants: &[Grant]) -> bool {
        grants.iter().any(|g| match g.kind {
            GranteeKind::Subject => g.id == self.subject,
            GranteeKind::Department => self.department.as_deref() == Some(g.id.as_str()),
            GranteeKind::Workspace => self.workspaces.iter().any(|w| w == &g.id),
        })
    }
}

/// One row of the readable-document listing.
#[derive(Debug, Clone, Serialize)]
pub struct ReadableDocument {
    pub document_hk: String,
    /// `None` for a document the organization owns collectively (see
    /// [`Ops::record_document_owner`]).
    pub owner_subject: Option<String>,
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
}

/// A job waiting to be resumed, as the sweep claimed it.
#[derive(Debug, Clone)]
pub struct LeasedJob {
    pub job_id: Uuid,
    pub run_id: RunId,
    pub spec: JobSpec,
    pub resumes: i32,
}

/// What a revocation row says. `id` is a `jti`, a `sid` or a subject, per `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub kind: RevocationKind,
    pub id: String,
    /// Tokens issued before this are refused.
    pub not_before: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RevocationKind {
    /// One token, by `jti`.
    Jti,
    /// One session, by Keycloak's `sid` — what a back-channel logout names.
    Sid,
    /// Every token of one person issued before `not_before`.
    Subject,
}

impl RevocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RevocationKind::Jti => "jti",
            RevocationKind::Sid => "sid",
            RevocationKind::Subject => "subject",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jti" => Some(RevocationKind::Jti),
            "sid" => Some(RevocationKind::Sid),
            "subject" => Some(RevocationKind::Subject),
            _ => None,
        }
    }
}

/// What an erasure removed, per table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Erasure {
    /// Job rows whose `spec.owner` and `owner_subject` were scrubbed.
    pub jobs: u64,
    /// Access rows whose owner was cleared (the documents stay, and become the
    /// organization's).
    pub documents: u64,
    /// Grants to or by this person that were deleted.
    pub grants: u64,
}

impl Erasure {
    pub fn total(&self) -> u64 {
        self.jobs + self.documents + self.grants
    }
}

impl Ops {
    // ───────────── document access ─────────────

    /// Records who uploaded a document, once.
    ///
    /// Called by the ingest stage, which is where the document hash key first exists. It
    /// does not overwrite: a second run of the same document keeps the first uploader and
    /// whatever visibility has been set since, because a re-upload is not a change of
    /// ownership. The returned bool says whether this call created the row.
    pub async fn record_document_owner(
        &self,
        tenant: &TenantId,
        document_hk: &HashKey,
        owner: Option<&Owner>,
        default_visibility: Visibility,
    ) -> Result<bool, VaultError> {
        let n = sqlx::query(
            "INSERT INTO ops.document_access
                 (tenant_id, document_hk, owner_subject, owner_department, workspaces, visibility)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (tenant_id, document_hk) DO NOTHING",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .bind(owner.map(|o| o.subject.as_str()))
        .bind(owner.and_then(|o| o.department.as_deref()))
        .bind(owner.map(|o| o.workspaces.clone()).unwrap_or_default())
        .bind(default_visibility.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// A document's access row and its grants. `None` when no row exists.
    pub async fn document_access(&self, tenant: &TenantId, document_hk: &HashKey) -> Result<Option<DocumentAccess>, VaultError> {
        let Some(r) = sqlx::query(
            "SELECT owner_subject, owner_department, workspaces, visibility, updated_at
             FROM ops.document_access WHERE tenant_id = $1 AND document_hk = $2",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let visibility: String = r.try_get("visibility")?;
        let grants = self.document_grants(tenant, document_hk).await?;
        Ok(Some(DocumentAccess {
            document_hk: document_hk.to_string(),
            owner_subject: r.try_get("owner_subject")?,
            owner_department: r.try_get("owner_department")?,
            workspaces: r.try_get("workspaces")?,
            // A value outside the enum cannot reach here: the column has a CHECK
            // constraint, and this module is the only writer.
            visibility: Visibility::parse(&visibility).unwrap_or(Visibility::Organization),
            grants,
            updated_at: r.try_get("updated_at")?,
        }))
    }

    pub async fn document_grants(&self, tenant: &TenantId, document_hk: &HashKey) -> Result<Vec<Grant>, VaultError> {
        sqlx::query(
            "SELECT grantee_kind, grantee_id, created_at FROM ops.document_grants
             WHERE tenant_id = $1 AND document_hk = $2 ORDER BY grantee_kind, grantee_id",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| {
            let kind: String = r.try_get("grantee_kind")?;
            Ok(Grant {
                kind: GranteeKind::parse(&kind).unwrap_or(GranteeKind::Subject),
                id: r.try_get("grantee_id")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect()
    }

    /// Changes how wide a document is open. `false` if it has no access row.
    pub async fn set_document_visibility(&self, tenant: &TenantId, document_hk: &HashKey, v: Visibility) -> Result<bool, VaultError> {
        let n = sqlx::query(
            "UPDATE ops.document_access SET visibility = $3, updated_at = now()
             WHERE tenant_id = $1 AND document_hk = $2",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .bind(v.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Adds a share. Idempotent: granting twice is one row.
    pub async fn grant_document(
        &self,
        tenant: &TenantId,
        document_hk: &HashKey,
        kind: GranteeKind,
        grantee: &str,
        granted_by: Option<&str>,
    ) -> Result<(), VaultError> {
        sqlx::query(
            "INSERT INTO ops.document_grants (tenant_id, document_hk, grantee_kind, grantee_id, granted_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant_id, document_hk, grantee_kind, grantee_id) DO NOTHING",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .bind(kind.as_str())
        .bind(grantee)
        .bind(granted_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Withdraws a share. `false` if there was none.
    pub async fn revoke_document_grant(
        &self,
        tenant: &TenantId,
        document_hk: &HashKey,
        kind: GranteeKind,
        grantee: &str,
    ) -> Result<bool, VaultError> {
        let n = sqlx::query(
            "DELETE FROM ops.document_grants
             WHERE tenant_id = $1 AND document_hk = $2 AND grantee_kind = $3 AND grantee_id = $4",
        )
        .bind(tenant.as_str())
        .bind(document_hk.as_str())
        .bind(kind.as_str())
        .bind(grantee)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// The access rows for several documents at once, so a listing does not issue one
    /// query per row. Documents without a row are absent from the map.
    pub async fn document_access_many(&self, tenant: &TenantId, hks: &[String]) -> Result<std::collections::HashMap<String, DocumentAccess>, VaultError> {
        if hks.is_empty() {
            return Ok(Default::default());
        }
        let mut out: std::collections::HashMap<String, DocumentAccess> = sqlx::query(
            "SELECT document_hk, owner_subject, owner_department, workspaces, visibility, updated_at
             FROM ops.document_access WHERE tenant_id = $1 AND document_hk = ANY($2)",
        )
        .bind(tenant.as_str())
        .bind(hks)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| {
            let hk: String = r.try_get("document_hk")?;
            let hk = hk.trim_end().to_string();
            let visibility: String = r.try_get("visibility")?;
            Ok((
                hk.clone(),
                DocumentAccess {
                    document_hk: hk,
                    owner_subject: r.try_get("owner_subject")?,
                    owner_department: r.try_get("owner_department")?,
                    workspaces: r.try_get("workspaces")?,
                    visibility: Visibility::parse(&visibility).unwrap_or(Visibility::Organization),
                    grants: Vec::new(),
                    updated_at: r.try_get("updated_at")?,
                },
            ))
        })
        .collect::<Result<_, sqlx::Error>>()?;
        for r in sqlx::query(
            "SELECT document_hk, grantee_kind, grantee_id, created_at FROM ops.document_grants
             WHERE tenant_id = $1 AND document_hk = ANY($2) ORDER BY grantee_kind, grantee_id",
        )
        .bind(tenant.as_str())
        .bind(hks)
        .fetch_all(&self.pool)
        .await?
        {
            let hk: String = r.try_get("document_hk")?;
            let kind: String = r.try_get("grantee_kind")?;
            if let Some(a) = out.get_mut(hk.trim_end()) {
                a.grants.push(Grant {
                    kind: GranteeKind::parse(&kind).unwrap_or(GranteeKind::Subject),
                    id: r.try_get("grantee_id")?,
                    created_at: r.try_get("created_at")?,
                });
            }
        }
        Ok(out)
    }

    /// One page of the documents this reader may read, newest first.
    ///
    /// The predicate is in SQL so the database does the narrowing, and then every row is
    /// put through [`Reader::may_read`] before it is returned. Two implementations of one
    /// rule is a divergence waiting to happen, so only one of them *decides*: the SQL is
    /// an index-friendly filter, and the Rust is the answer. A row the SQL lets through
    /// and `may_read` refuses is dropped — which is why the count comes from the rows and
    /// not from a `count(*)` with the same predicate.
    pub async fn readable_documents(
        &self,
        tenant: &TenantId,
        reader: &Reader,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<ReadableDocument>, VaultError> {
        let page_size = page_size.clamp(1, 500);
        let rows = sqlx::query(
            "SELECT a.document_hk, a.owner_subject, a.owner_department, a.workspaces, a.visibility, a.updated_at,
                    a.created_at
               FROM ops.document_access a
              WHERE a.tenant_id = $1
                AND ($2
                     OR a.owner_subject IS NULL
                     OR a.owner_subject = $3
                     OR a.visibility = 'organization'
                     OR (a.visibility = 'department' AND a.owner_department IS NOT NULL AND a.owner_department = $4)
                     OR (a.visibility = 'workspace' AND a.workspaces && $5)
                     OR EXISTS (SELECT 1 FROM ops.document_grants g
                                 WHERE g.tenant_id = a.tenant_id AND g.document_hk = a.document_hk
                                   AND ((g.grantee_kind = 'subject' AND g.grantee_id = $3)
                                     OR (g.grantee_kind = 'department' AND g.grantee_id = $4)
                                     OR (g.grantee_kind = 'workspace' AND g.grantee_id = ANY($5)))))
              ORDER BY a.created_at DESC
              LIMIT $6 OFFSET $7",
        )
        .bind(tenant.as_str())
        .bind(reader.admin)
        .bind(&reader.subject)
        .bind(reader.department.as_deref())
        .bind(&reader.workspaces)
        .bind(i64::from(page_size))
        .bind(i64::from(page) * i64::from(page_size))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let hk: String = r.try_get("document_hk")?;
            let hk = hk.trim_end().to_string();
            let visibility: String = r.try_get("visibility")?;
            let access = DocumentAccess {
                document_hk: hk.clone(),
                owner_subject: r.try_get("owner_subject")?,
                owner_department: r.try_get("owner_department")?,
                workspaces: r.try_get("workspaces")?,
                visibility: Visibility::parse(&visibility).unwrap_or(Visibility::Organization),
                grants: Vec::new(),
                updated_at: r.try_get("updated_at")?,
            };
            // The SQL narrowed; this decides. A grant is the one case the predicate
            // above can admit without `may_read` seeing it, so those rows are re-checked
            // with their grants loaded rather than trusted.
            let allowed = reader.may_read(Some(&access)) || {
                let with_grants = self.document_access(tenant, &hk_from(&hk)?).await?;
                reader.may_read(with_grants.as_ref())
            };
            if allowed {
                out.push(ReadableDocument {
                    document_hk: hk,
                    owner_subject: access.owner_subject,
                    visibility: access.visibility,
                    created_at: r.try_get("created_at")?,
                });
            }
        }
        Ok(out)
    }

    // ───────────── job leases and resume ─────────────

    /// Takes the lease on a job this process is about to run.
    ///
    /// `false` when another copy holds an unexpired lease, which is what stops two copies
    /// from processing one document: the writes are idempotent either way, but the model
    /// calls are not free.
    pub async fn take_lease(&self, job_id: Uuid, holder: &str, ttl: Duration) -> Result<bool, VaultError> {
        let n = sqlx::query(
            "UPDATE ops.jobs SET lease_owner = $2, lease_expires_at = now() + ($3::bigint * interval '1 second')
             WHERE job_id = $1 AND (lease_owner IS NULL OR lease_owner = $2 OR lease_expires_at < now())",
        )
        .bind(job_id)
        .bind(holder)
        .bind(secs(ttl))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    /// Extends a lease this process holds. `false` if it has been taken by someone else,
    /// which is the signal to stop working on the document.
    pub async fn renew_lease(&self, job_id: Uuid, holder: &str, ttl: Duration) -> Result<bool, VaultError> {
        let n = sqlx::query(
            "UPDATE ops.jobs SET lease_expires_at = now() + ($3::bigint * interval '1 second')
             WHERE job_id = $1 AND lease_owner = $2",
        )
        .bind(job_id)
        .bind(holder)
        .bind(secs(ttl))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }

    /// Drops a lease, so a job this process gave up on is resumable immediately rather
    /// than after the lease expires.
    pub async fn release_lease(&self, job_id: Uuid, holder: &str) -> Result<(), VaultError> {
        sqlx::query("UPDATE ops.jobs SET lease_owner = NULL, lease_expires_at = NULL WHERE job_id = $1 AND lease_owner = $2")
            .bind(job_id)
            .bind(holder)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Claims up to `limit` abandoned jobs and leases them to `holder`.
    ///
    /// This is what replaces the queue's redelivery (D13): a job is abandoned when the
    /// process carrying it died, which shows as an unfinished state and a lease that
    /// stopped being renewed. `FOR UPDATE SKIP LOCKED` is what makes it safe to run in
    /// every copy at once — two sweeps never claim the same job, and the lease then keeps
    /// them apart for longer than the transaction.
    ///
    /// `max_resumes` bounds how often one job may be picked up, so a document that kills
    /// the process that touches it cannot take the fleet down one copy at a time.
    pub async fn claim_abandoned_jobs(
        &self,
        holder: &str,
        limit: i64,
        ttl: Duration,
        max_resumes: i32,
    ) -> Result<Vec<LeasedJob>, VaultError> {
        let rows = sqlx::query(
            "WITH claimed AS (
                 SELECT job_id FROM ops.jobs
                 WHERE state NOT IN ('done', 'partial', 'failed')
                   AND (lease_expires_at IS NULL OR lease_expires_at < now())
                   AND resumes < $4
                 ORDER BY created_at
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE ops.jobs j
                SET lease_owner = $1, lease_expires_at = now() + ($3::bigint * interval '1 second'), resumes = j.resumes + 1
               FROM claimed
              WHERE j.job_id = claimed.job_id
             RETURNING j.job_id, j.run_id, j.spec, j.resumes",
        )
        .bind(holder)
        .bind(limit)
        .bind(secs(ttl))
        .bind(max_resumes)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let spec: serde_json::Value = r.try_get("spec")?;
                Ok(LeasedJob {
                    job_id: r.try_get("job_id")?,
                    run_id: RunId(r.try_get("run_id")?),
                    spec: serde_json::from_value(spec).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    resumes: r.try_get("resumes")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(VaultError::from)
    }

    /// Fails the jobs that have been resumed as often as they may be, so a job that
    /// cannot be finished stops being claimed and the caller is told.
    pub async fn fail_exhausted_jobs(&self, max_resumes: i32) -> Result<u64, VaultError> {
        let n = sqlx::query(
            "UPDATE ops.jobs SET state = 'failed', error = $2, updated_at = now(),
                                 lease_owner = NULL, lease_expires_at = NULL
             WHERE state NOT IN ('done', 'partial', 'failed')
               AND resumes >= $1
               AND (lease_expires_at IS NULL OR lease_expires_at < now())",
        )
        .bind(max_resumes)
        .bind(format!("abandoned after {max_resumes} resume attempts; re-upload to try again"))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    // ───────────── token revocation ─────────────

    /// Records a revocation. Re-revoking moves `not_before` forward rather than failing.
    pub async fn revoke_token(
        &self,
        kind: RevocationKind,
        id: &str,
        tenant: Option<&TenantId>,
        not_before: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        reason: Option<&str>,
    ) -> Result<(), VaultError> {
        sqlx::query(
            "INSERT INTO ops.revoked_tokens (kind, id, tenant_id, not_before, expires_at, reason)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (kind, id) DO UPDATE
                SET not_before = GREATEST(ops.revoked_tokens.not_before, EXCLUDED.not_before),
                    expires_at = GREATEST(ops.revoked_tokens.expires_at, EXCLUDED.expires_at),
                    reason     = COALESCE(EXCLUDED.reason, ops.revoked_tokens.reason)",
        )
        .bind(kind.as_str())
        .bind(id)
        .bind(tenant.map(|t| t.as_str()))
        .bind(not_before)
        .bind(expires_at)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every revocation that has not expired. Read into the verifier's cache, because
    /// checking one row per request would put Postgres on the path of every call — which
    /// is the thing local signature verification exists to avoid (D10).
    pub async fn live_revocations(&self) -> Result<Vec<Revocation>, VaultError> {
        sqlx::query("SELECT kind, id, not_before FROM ops.revoked_tokens WHERE expires_at > now()")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| {
                let kind: String = r.try_get("kind")?;
                Ok(Revocation {
                    kind: RevocationKind::parse(&kind).unwrap_or(RevocationKind::Jti),
                    id: r.try_get("id")?,
                    not_before: r.try_get("not_before")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(VaultError::from)
    }

    /// Drops revocations whose tokens have all expired anyway.
    pub async fn purge_revocations(&self) -> Result<u64, VaultError> {
        Ok(sqlx::query("DELETE FROM ops.revoked_tokens WHERE expires_at <= now()").execute(&self.pool).await?.rows_affected())
    }

    // ───────────── retention and erasure ─────────────

    /// Removes one person's identifiers from the operational tables.
    ///
    /// This is the erasure path, and it is short because of where the personal data is
    /// *not*: no satellite in the vault holds an owner, so nothing has to be rewritten
    /// in an insert-only store. What it does:
    ///
    /// - jobs: `owner_subject` cleared and `spec.owner` removed, keeping the job itself —
    ///   the document was processed, and a tenant's own history of what it uploaded is
    ///   not the person's to erase;
    /// - documents: the owner fields cleared and visibility widened to the organization,
    ///   because a `private` document whose owner no longer exists would be readable by
    ///   nobody and recoverable by no one but an administrator;
    /// - grants: every grant to or by this person deleted.
    pub async fn erase_owner(&self, tenant: &TenantId, subject: &str) -> Result<Erasure, VaultError> {
        let mut tx = self.pool.begin().await?;
        let jobs = sqlx::query(
            "UPDATE ops.jobs
                SET owner_subject = NULL,
                    spec = spec - 'owner',
                    owner_erased_at = now(),
                    updated_at = updated_at
              WHERE tenant_id = $1 AND owner_subject = $2",
        )
        .bind(tenant.as_str())
        .bind(subject)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let documents = sqlx::query(
            "UPDATE ops.document_access
                SET owner_subject = NULL, owner_department = NULL, workspaces = '{}',
                    visibility = 'organization', updated_at = now()
              WHERE tenant_id = $1 AND owner_subject = $2",
        )
        .bind(tenant.as_str())
        .bind(subject)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let grants = sqlx::query(
            "DELETE FROM ops.document_grants
              WHERE tenant_id = $1 AND ((grantee_kind = 'subject' AND grantee_id = $2) OR granted_by = $2)",
        )
        .bind(tenant.as_str())
        .bind(subject)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(Erasure { jobs, documents, grants })
    }

    /// Scrubs the owner from finished jobs older than `retention`, keeping the job row.
    ///
    /// The subject is stored because per-person access control needs it *while the work
    /// is live*; once the job is finished and old, it is a personal identifier with no
    /// remaining purpose, which is exactly what storage limitation (GDPR Art. 5(1)(e))
    /// is about. The document's access row keeps the owner, because that is still in use.
    pub async fn scrub_old_job_owners(&self, retention: Duration) -> Result<u64, VaultError> {
        let n = sqlx::query(
            "UPDATE ops.jobs
                SET owner_subject = NULL, spec = spec - 'owner', owner_erased_at = now()
              WHERE state IN ('done', 'partial', 'failed')
                AND owner_erased_at IS NULL
                AND owner_subject IS NOT NULL
                AND updated_at < now() - ($1::bigint * interval '1 second')",
        )
        .bind(secs(retention))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    /// Sets the denormalised `owner_subject` on jobs this process just created. Called
    /// from `create_jobs`, which is why it takes the ids rather than the specs.
    pub(crate) async fn set_job_owners<'c>(
        tx: &mut sqlx::Transaction<'c, sqlx::Postgres>,
        job_ids: &[Uuid],
        subjects: &[Option<String>],
    ) -> Result<(), VaultError> {
        sqlx::query(
            "UPDATE ops.jobs j SET owner_subject = v.subject
               FROM UNNEST($1::uuid[], $2::text[]) AS v(job_id, subject)
              WHERE j.job_id = v.job_id",
        )
        .bind(job_ids)
        .bind(subjects)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(owner: &str, dept: Option<&str>, spaces: &[&str], visibility: Visibility) -> DocumentAccess {
        DocumentAccess {
            document_hk: "a".repeat(64),
            owner_subject: Some(owner.into()),
            owner_department: dept.map(str::to_string),
            workspaces: spaces.iter().map(|s| s.to_string()).collect(),
            visibility,
            grants: Vec::new(),
            updated_at: Utc::now(),
        }
    }

    fn reader(subject: &str, dept: Option<&str>, spaces: &[&str]) -> Reader {
        Reader {
            subject: subject.into(),
            department: dept.map(str::to_string),
            workspaces: spaces.iter().map(|s| s.to_string()).collect(),
            admin: false,
        }
    }

    #[test]
    fn a_private_document_is_the_owners_alone() {
        let a = access("alice", Some("research"), &["red"], Visibility::Private);
        assert!(reader("alice", None, &[]).may_read(Some(&a)), "the owner always reads it");
        assert!(!reader("bob", Some("research"), &["red"]).may_read(Some(&a)), "a colleague in the same department and workspace still cannot");
    }

    #[test]
    fn department_visibility_needs_both_sides_to_name_the_same_department() {
        let a = access("alice", Some("research"), &[], Visibility::Department);
        assert!(reader("bob", Some("research"), &[]).may_read(Some(&a)));
        assert!(!reader("bob", Some("sales"), &[]).may_read(Some(&a)));
        // The case worth stating: two people whose department claim is missing are not
        // colleagues, so `None == None` must not open the document.
        let no_dept = access("alice", None, &[], Visibility::Department);
        assert!(!reader("bob", None, &[]).may_read(Some(&no_dept)), "a missing department is not a shared one");
    }

    #[test]
    fn workspace_visibility_needs_one_workspace_in_common() {
        let a = access("alice", None, &["red", "blue"], Visibility::Workspace);
        assert!(reader("bob", None, &["green", "blue"]).may_read(Some(&a)));
        assert!(!reader("bob", None, &["green"]).may_read(Some(&a)));
        assert!(!reader("bob", None, &[]).may_read(Some(&a)));
    }

    #[test]
    fn a_grant_opens_a_document_its_visibility_would_not() {
        let mut a = access("alice", Some("research"), &["red"], Visibility::Private);
        a.grants.push(Grant { kind: GranteeKind::Subject, id: "bob".into(), created_at: Utc::now() });
        a.grants.push(Grant { kind: GranteeKind::Workspace, id: "green".into(), created_at: Utc::now() });
        a.grants.push(Grant { kind: GranteeKind::Department, id: "sales".into(), created_at: Utc::now() });
        assert!(reader("bob", None, &[]).may_read(Some(&a)), "granted by name");
        assert!(reader("carol", None, &["green"]).may_read(Some(&a)), "granted a workspace");
        assert!(reader("dave", Some("sales"), &[]).may_read(Some(&a)), "granted a department");
        assert!(!reader("erin", Some("research"), &["red"]).may_read(Some(&a)), "the owner's own department and workspace are not a grant");
    }

    #[test]
    fn a_document_nothing_recorded_an_owner_for_stays_the_organizations() {
        // What every document uploaded before this existed looks like. Locking a tenant
        // out of its own history would be a worse answer than leaving it as it was.
        assert!(reader("anyone", None, &[]).may_read(None), "no access row at all");
        let mut orphan = access("alice", None, &[], Visibility::Private);
        orphan.owner_subject = None;
        assert!(reader("anyone", None, &[]).may_read(Some(&orphan)), "a row whose owner was erased");
    }

    #[test]
    fn an_administrator_reads_everything_in_the_organization() {
        let a = access("alice", Some("research"), &[], Visibility::Private);
        let mut admin = reader("op", None, &[]);
        admin.admin = true;
        assert!(admin.may_read(Some(&a)));
    }

    #[test]
    fn organization_visibility_is_what_every_document_used_to_be() {
        let a = access("alice", Some("research"), &["red"], Visibility::Organization);
        assert!(reader("stranger", None, &[]).may_read(Some(&a)));
    }

    #[test]
    fn visibility_and_grantee_names_round_trip() {
        for v in [Visibility::Private, Visibility::Department, Visibility::Workspace, Visibility::Organization] {
            assert_eq!(Visibility::parse(v.as_str()), Some(v));
        }
        assert_eq!(Visibility::parse("everyone"), None);
        for k in [GranteeKind::Subject, GranteeKind::Department, GranteeKind::Workspace] {
            assert_eq!(GranteeKind::parse(k.as_str()), Some(k));
        }
        for k in [RevocationKind::Jti, RevocationKind::Sid, RevocationKind::Subject] {
            assert_eq!(RevocationKind::parse(k.as_str()), Some(k));
        }
    }
}
