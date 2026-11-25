//! Read scoping inside an organization: the check every read goes through, the routes
//! that change who may read what, and the two routes next to them — erasure and token
//! revocation.
//!
//! The organization is still the isolation boundary and nothing here touches it: a
//! caller cannot name another organization, and every key is still hashed with it (D11).
//! What these routes decide is narrower and was previously not decided at all — which
//! *member* of an organization may read which of its documents.
//!
//! The rule itself is [`ds_vault::access::Reader::may_read`], in the crate that owns the
//! rows. This module is where a request meets it.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use ds_core::HashKey;
use ds_vault::access::{Erasure, GranteeKind, ReadableDocument, RevocationKind, Visibility};

use crate::auth::{Admin, Caller, Principal, REVOCATION_TTL};
use crate::{internal, parse_hk, ApiError, AppState};

/// Whether this caller may read this document, and the 403 if not.
///
/// A `403` and not a `404`: the document hash key is derived from the content's SHA-256
/// (`HashKey::document`), so a caller who can name one already has the bytes. Hiding that
/// it exists would protect nothing and would leave a colleague unable to tell that asking
/// the owner for access is the thing to do.
pub async fn require_read(s: &AppState, caller: &Principal, hk: &HashKey) -> Result<(), ApiError> {
    let access = s.ops.document_access(&caller.tenant, hk).await.map_err(internal)?;
    let reader = caller.reader(s.api.admin_reads_all);
    if reader.may_read(access.as_ref()) {
        return Ok(());
    }
    metrics::counter!("ds_api_access_refusals_total", "object" => "document").increment(1);
    // No subject in the log line: which person was refused is Keycloak's to record
    // (`docs/SPEC.md` §5.6). The document and the organization are ours.
    tracing::info!(tenant = %caller.tenant.as_str(), document_hk = %hk, "read refused by document scope");
    Err((StatusCode::FORBIDDEN, "not readable by this caller; ask the document's owner to share it".into()))
}

/// Whether this caller may *change* who reads a document: its owner, or an administrator.
///
/// `admin_reads_all` does not come into it. Reading a document and re-sharing it are
/// different powers, and a deployment that separates administration from access still
/// needs someone able to fix a mis-shared document.
async fn require_own(s: &AppState, caller: &Principal, hk: &HashKey) -> Result<(), ApiError> {
    let access = s.ops.document_access(&caller.tenant, hk).await.map_err(internal)?;
    let Some(a) = access else {
        return Err((
            StatusCode::NOT_FOUND,
            "no access record for this document; it predates per-person scoping and is the organization's".into(),
        ));
    };
    if caller.admin || a.owner_subject.as_deref() == Some(caller.subject.as_str()) {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, "only the document's owner or an administrator can change who may read it".into()))
}

// ───────────── reading and changing a document's access ─────────────

/// `GET /v1/documents/{hk}/access`
pub async fn get_access(State(s): State<AppState>, Caller(caller): Caller, Path(hk): Path<String>) -> Result<Json<Value>, ApiError> {
    let hk = parse_hk(&hk)?;
    require_read(&s, &caller, &hk).await?;
    match s.ops.document_access(&caller.tenant, &hk).await.map_err(internal)? {
        Some(a) => Ok(Json(json!({
            "document_hk": a.document_hk,
            // The owner is named only to the owner and to an administrator. To everyone
            // else it is a subject — a personal identifier — handed out by an API that
            // has no reason to.
            "owner": (caller.admin || a.owner_subject.as_deref() == Some(caller.subject.as_str())).then(|| a.owner_subject.clone()),
            "visibility": a.visibility,
            "workspaces": a.workspaces,
            "grants": a.grants,
            "updated_at": a.updated_at,
        }))),
        // Readable, and with nothing to say about who else can: a document from before
        // scoping existed.
        None => Ok(Json(json!({ "document_hk": hk.to_string(), "visibility": Visibility::Organization, "legacy": true }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct VisibilityBody {
    visibility: String,
}

/// `PUT /v1/documents/{hk}/access`
pub async fn set_access(
    State(s): State<AppState>,
    Caller(caller): Caller,
    Path(hk): Path<String>,
    Json(body): Json<VisibilityBody>,
) -> Result<Json<Value>, ApiError> {
    let hk = parse_hk(&hk)?;
    require_own(&s, &caller, &hk).await?;
    let v = Visibility::parse(body.visibility.trim())
        .ok_or((StatusCode::BAD_REQUEST, "visibility must be private, department, workspace or organization".to_string()))?;
    if !s.ops.set_document_visibility(&caller.tenant, &hk, v).await.map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "document has no access record".into()));
    }
    metrics::counter!("ds_api_visibility_changes_total", "visibility" => v.as_str()).increment(1);
    Ok(Json(json!({ "document_hk": hk.to_string(), "visibility": v })))
}

#[derive(Debug, Deserialize)]
pub struct GrantBody {
    kind: String,
    id: String,
}

/// `POST /v1/documents/{hk}/grants`
pub async fn add_grant(
    State(s): State<AppState>,
    Caller(caller): Caller,
    Path(hk): Path<String>,
    Json(body): Json<GrantBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let hk = parse_hk(&hk)?;
    require_own(&s, &caller, &hk).await?;
    let (kind, id) = grantee(&body.kind, &body.id)?;
    s.ops.grant_document(&caller.tenant, &hk, kind, &id, Some(&caller.subject)).await.map_err(internal)?;
    metrics::counter!("ds_api_grants_total", "kind" => kind.as_str()).increment(1);
    Ok((StatusCode::CREATED, Json(json!({ "document_hk": hk.to_string(), "kind": kind, "id": id }))))
}

/// `DELETE /v1/documents/{hk}/grants/{kind}/{id}`
pub async fn remove_grant(
    State(s): State<AppState>,
    Caller(caller): Caller,
    Path((hk, kind, id)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let hk = parse_hk(&hk)?;
    require_own(&s, &caller, &hk).await?;
    let (kind, id) = grantee(&kind, &id)?;
    if s.ops.revoke_document_grant(&caller.tenant, &hk, kind, &id).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no such grant".into()))
    }
}

/// Validates a grantee. The id is bounded because it is stored and compared, and a
/// department or workspace name is a short label rather than a document.
fn grantee(kind: &str, id: &str) -> Result<(GranteeKind, String), ApiError> {
    let kind = GranteeKind::parse(kind.trim()).ok_or((StatusCode::BAD_REQUEST, "kind must be subject, department or workspace".to_string()))?;
    let id = id.trim();
    if id.is_empty() || id.len() > 256 {
        return Err((StatusCode::BAD_REQUEST, "grantee id must be 1..256 characters".into()));
    }
    Ok((kind, id.to_string()))
}

// ───────────── what this caller can read ─────────────

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    page: u32,
    #[serde(default = "page_size")]
    page_size: u32,
}

fn page_size() -> u32 {
    100
}

#[derive(Serialize)]
pub struct DocumentList {
    page: u32,
    page_size: u32,
    documents: Vec<ReadableDocument>,
}

/// `GET /v1/documents` — the documents this caller may read.
///
/// New with scoping, and the reason it is worth a route: once a member cannot
/// necessarily read everything, "what can I read" stops being answerable by the caller
/// and has to be answered here.
pub async fn list_documents(
    State(s): State<AppState>,
    Caller(caller): Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<DocumentList>, ApiError> {
    let reader = caller.reader(s.api.admin_reads_all);
    let documents = s.ops.readable_documents(&caller.tenant, &reader, q.page, q.page_size).await.map_err(internal)?;
    Ok(Json(DocumentList { page: q.page, page_size: q.page_size.clamp(1, 500), documents }))
}

// ───────────── erasure ─────────────

#[derive(Serialize)]
pub struct Erased {
    subject_erased: bool,
    #[serde(flatten)]
    counts: Erasure,
}

/// `DELETE /v1/owners/{subject}` — removes one person's identifiers from the operational
/// tables.
///
/// Allowed to an administrator of the organization, and to the person themselves: an
/// erasure request is the data subject's to make, and requiring an administrator to relay
/// it would be a worse answer than accepting it from a verified token that *is* them.
///
/// What it does not touch, because it does not have to: the vault. No satellite stores an
/// owner, which is what makes this a short operation on a store that is insert-only by
/// design — see [`ds_vault::access::Ops::erase_owner`].
pub async fn erase_owner(State(s): State<AppState>, Caller(caller): Caller, Path(subject): Path<String>) -> Result<Json<Erased>, ApiError> {
    let subject = subject.trim().to_string();
    if subject.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "subject must not be empty".into()));
    }
    if !caller.admin && subject != caller.subject {
        return Err((StatusCode::FORBIDDEN, "only an administrator can erase another person".into()));
    }
    let counts = s.ops.erase_owner(&caller.tenant, &subject).await.map_err(internal)?;
    metrics::counter!("ds_api_erasures_total").increment(1);
    tracing::info!(tenant = %caller.tenant.as_str(), jobs = counts.jobs, documents = counts.documents, grants = counts.grants, "owner erased");
    Ok(Json(Erased { subject_erased: counts.total() > 0, counts }))
}

/// Scrubs the caller's subject from jobs that finished longer ago than
/// `api.job_retention_days`, every six hours.
///
/// The subject is written to `ops.jobs.spec` because per-person access control needs it
/// while the work is live. Once the job is finished and old it is a personal identifier
/// with no remaining purpose, which is what storage limitation (GDPR Art. 5(1)(e)) is
/// about — so there is now something that removes it, rather than a documented intention
/// to.
pub fn spawn_retention_sweep(ops: Arc<ds_vault::ops::Ops>, retention_days: u64) {
    if retention_days == 0 {
        tracing::warn!("api.job_retention_days = 0: the caller's subject is kept on finished jobs indefinitely");
        return;
    }
    let retention = std::time::Duration::from_secs(retention_days * 86_400);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            tick.tick().await;
            match ops.scrub_old_job_owners(retention).await {
                Ok(0) => {}
                Ok(n) => {
                    metrics::counter!("ds_api_jobs_scrubbed_total").increment(n);
                    tracing::info!(jobs = n, retention_days, "scrubbed the caller's subject from finished jobs");
                }
                Err(e) => tracing::warn!(error = %e, "job retention sweep failed"),
            }
        }
    });
}

// ───────────── token revocation ─────────────

#[derive(Debug, Deserialize)]
pub struct RevokeBody {
    /// `jti`, `sid` or `subject`.
    kind: String,
    id: String,
    reason: Option<String>,
}

/// `POST /v1/auth/revocations` — stops accepting a token, a session or a person before
/// the token's own expiry.
///
/// An administrator may revoke anything in the organization; anyone may revoke their own
/// session or their own subject, which is what "log me out everywhere" needs.
pub async fn revoke(
    State(s): State<AppState>,
    Caller(caller): Caller,
    Json(body): Json<RevokeBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let kind = RevocationKind::parse(body.kind.trim()).ok_or((StatusCode::BAD_REQUEST, "kind must be jti, sid or subject".to_string()))?;
    let id = body.id.trim();
    if id.is_empty() || id.len() > 256 {
        return Err((StatusCode::BAD_REQUEST, "id must be 1..256 characters".into()));
    }
    let is_own = match kind {
        RevocationKind::Subject => id == caller.subject,
        RevocationKind::Sid => caller.session_id.as_deref() == Some(id),
        RevocationKind::Jti => caller.token_id.as_deref() == Some(id),
    };
    if !caller.admin && !is_own {
        return Err((StatusCode::FORBIDDEN, "only an administrator can revoke someone else's token".into()));
    }
    let now = chrono::Utc::now();
    s.auth
        .revoke(kind, id, Some(&caller.tenant), now, now + REVOCATION_TTL, body.reason.as_deref())
        .await
        .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(json!({ "kind": kind, "revoked_until": now + REVOCATION_TTL }))))
}

/// `GET /v1/auth/revocations` — how many revocations this copy is holding.
///
/// Deliberately a count and not a list: the list is `sub`s and session ids, which is
/// personal data this API has no reason to hand out, and the operational question is
/// whether revocation is working at all.
pub async fn revocation_status(State(s): State<AppState>, _: Admin) -> Json<Value> {
    Json(json!({ "revocations": s.auth.revocation_count().await, "refresh_s": s.api.revocation_refresh_s }))
}

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    logout_token: String,
}

/// `POST /v1/auth/backchannel-logout` — OpenID Connect Back-Channel Logout 1.0 §2.5.
///
/// **Unauthenticated by design, and that is not a hole**: the request carries no bearer
/// token because Keycloak is not a bearer-token client here — it carries a *logout token*
/// signed by the realm, and the signature over the realm's published keys is the
/// authentication. An unsigned or stale one is refused, which is the whole of what this
/// route accepts.
///
/// It answers 200 even when there is nothing to do, because the specification asks for a
/// 200 on success and a logout that names a session this copy has never seen *is*
/// success: the session is not accepted here either way.
pub async fn backchannel_logout(State(s): State<AppState>, form: axum::extract::Form<LogoutForm>) -> Response {
    let req = match s.auth.verify_logout_token(&form.logout_token).await {
        Ok(r) => r,
        Err(e) => {
            metrics::counter!("ds_api_logout_tokens_total", "outcome" => "refused").increment(1);
            tracing::warn!(reason = e.reason(), "back-channel logout refused");
            // §2.5: errors are 400 with a JSON body, and cache-control is no-store
            // because a cached logout response would be answered without this check.
            return (
                StatusCode::BAD_REQUEST,
                [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
                Json(json!({ "error": "invalid_request", "error_description": e.reason() })),
            )
                .into_response();
        }
    };
    if let Err(e) = s.auth.apply_logout(&req).await {
        tracing::error!(error = %e, "could not record a verified back-channel logout");
        // The realm did its part; we could not do ours, and it should retry.
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))], "retry").into_response();
    }
    metrics::counter!("ds_api_logout_tokens_total", "outcome" => "applied").increment(1);
    tracing::info!(session = req.session_id.is_some(), "back-channel logout applied");
    (StatusCode::OK, [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))], "").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grantee_is_bounded_and_named() {
        assert!(grantee("subject", "u1").is_ok());
        assert!(grantee(" workspace ", " red ").is_ok());
        assert_eq!(grantee("workspace", " red ").expect("trimmed").1, "red");
        assert!(grantee("everyone", "u1").is_err(), "an unknown kind is not a grant");
        assert!(grantee("subject", "   ").is_err());
        assert!(grantee("subject", &"x".repeat(257)).is_err(), "an unbounded grantee id");
    }
}
