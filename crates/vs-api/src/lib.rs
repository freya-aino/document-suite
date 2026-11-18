//! The public HTTP API, as a library. See `SPEC.md`.
//!
//! The routes and their wiring live here rather than in a binary because the suite ships
//! as **one** service: `vs-suite` serves these routes and runs the worker roles in the
//! same process, so neither half can be the thing that owns `main`. What a deployment
//! must provide is in `deploy/README.md`.

// Handlers return `Response` in the error position for headers like `Retry-After`.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, MatchedPath, Multipart, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use vs_core::{HashKey, JobSpec, JobState, LanguageTag, Owner, Priority, Profile, RunId, TenantId};
use vs_dag::StepRegistry;
use vs_infra::blob::{key_from_uri, keys, BlobError, BlobStore};
use vs_infra::config::ApiConfig;
use vs_vault::ops::{NewJob, Ops};

pub mod auth;
use auth::{Admin, Auth, Caller, Tenant};

/// Documents per `POST /v1/batches`.
const MAX_BATCH_DOCUMENTS: usize = 10_000;
/// Concurrent blob HEADs / publishes while handling a batch.
const BATCH_CONCURRENCY: usize = 32;
/// Seconds clients should wait after a 429.
const RETRY_AFTER_S: &str = "30";

/// Everything the routes need. Built by `vs-suite`, which owns the connections the
/// worker roles share with them.
#[derive(Clone)]
pub struct AppState {
    pub ops: Arc<Ops>,
    /// Token verification. Every route but `/healthz` and `/readyz` goes through it.
    pub auth: Arc<Auth>,
    /// Where an accepted upload goes. No queue: this hands it to the pipeline running in
    /// this same process.
    pub dispatch: Arc<dyn Dispatch>,
    pub blob: Arc<dyn BlobStore>,
    pub bucket: String,
    pub api: ApiConfig,
    /// Production step registry for DAG-list validation; `None` if the model registry
    /// could not be loaded at startup (DAG-list uploads then get 503).
    pub steps: Option<Arc<StepRegistry>>,
    pub metrics: PrometheusHandle,
}

#[derive(Debug, Deserialize)]
struct UploadOptions {
    language: LanguageTag,
    #[serde(default)]
    priority: Priority,
    #[serde(default)]
    profile: Profile,
    dag_list_version: Option<String>,
    external_ref: Option<String>,
    /// Optional: the caller already knows what these documents are.
    doc_type: Option<vs_core::DocType>,
    /// JSON variant: object already in the bucket.
    source_uri: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DocumentQuery {
    #[serde(default)]
    skip_if_processed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Accepted {
    job_id: uuid::Uuid,
    document_hk: Option<String>,
    run_id: uuid::Uuid,
    status_url: String,
}

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
}

/// How an accepted upload reaches the pipeline.
///
/// A trait because these routes must not depend on the pipeline: this crate is a library
/// and [`vs-suite`] is what owns both halves. It is also the seam a test substitutes.
///
/// `dispatch` returns when the document is *accepted*, not when it is finished — the
/// response is a `202` and a job id either way. There is no queue behind it any more, so
/// the implementation is what bounds concurrency, and `in_flight` is what admission
/// control reads.
///
/// [`vs-suite`]: https://example.invalid
#[async_trait::async_trait]
pub trait Dispatch: Send + Sync {
    async fn dispatch(&self, job_id: uuid::Uuid, run_id: RunId, spec: JobSpec) -> anyhow::Result<()>;
    /// Documents accepted and not yet finished in this process.
    fn in_flight(&self) -> u64;
}

/// Deletes expired idempotency keys, hourly.
///
/// Part of serving these routes rather than of the binary that serves them: the keys are
/// this crate's table, and a deployment that forgot to start this would grow it forever.
pub fn spawn_idempotency_purge(ops: Arc<Ops>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = ops.purge_idempotency_keys().await {
                tracing::warn!(error = %e, "idempotency purge failed");
            }
        }
    });
}

/// Every route, with authentication over all but the health probes.
pub fn router(state: AppState) -> Router {
    // Body limit for uploads: the handler enforces `max_upload_bytes` on the file part
    // itself; the slack covers the other multipart parts.
    let upload_limit = usize::try_from(state.api.max_upload_bytes.saturating_add(1024 * 1024)).unwrap_or(usize::MAX);

    // Everything that reads or writes a tenant's work, plus `/metrics`: behind one
    // authentication layer rather than per-handler checks, so adding a route cannot
    // quietly add an unauthenticated one.
    //
    // `/metrics` is in here because this API is reachable from outside, where an
    // unauthenticated metrics endpoint publishes request volumes, upload sizes and error
    // rates to anyone who asks. A Prometheus scrape authenticates as a Keycloak service
    // account holding the admin role — see SPEC.md.
    let protected = Router::new()
        .route("/v1/documents", post(create_document).layer(DefaultBodyLimit::max(upload_limit)))
        .route("/v1/batches", post(create_batch).layer(DefaultBodyLimit::max(16 * 1024 * 1024)))
        .route("/v1/jobs/{job_id}", get(get_job))
        .route("/v1/batches/{batch_id}", get(get_batch))
        .route("/v1/documents/{document_hk}", get(get_document))
        .route("/v1/documents/{document_hk}/pages/{page_no}", get(get_page))
        .route("/v1/dag-lists", get(list_dag_lists).post(upload_dag_list))
        .route("/v1/dag-lists/{version}/activate", post(activate_dag_list))
        .route("/metrics", get(metrics_handler))
        .route_layer(middleware::from_fn_with_state(state.auth.clone(), auth::authenticate));

    // Liveness and readiness only. A load balancer cannot hold a token, and neither
    // says anything about a tenant.
    let public = Router::new().route("/healthz", get(|| async { "ok" })).route("/readyz", get(readyz));

    protected.merge(public).layer(middleware::from_fn(record_metrics)).with_state(state)
}

// ───────────── metrics ─────────────

async fn record_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req.extensions().get::<MatchedPath>().map(|p| p.as_str().to_string()).unwrap_or_else(|| "unmatched".into());
    // The span is created here, outside the authentication layer, with the tenant empty:
    // `auth::authenticate` fills it in once it knows which organization called, and every
    // log line under the request then carries it.
    //
    // The *person* is deliberately absent. Telemetry carries no user-specific identifier
    // (`docs/SPEC.md` §5.6): spans are exported to a collector and logs are shipped, both
    // with their own retention, and a Keycloak `sub` in them is personal data in a place
    // that was never designed to hold any. Which person made a request is answered by
    // Keycloak's own audit log, which is the system that is allowed to know.
    let span = tracing::info_span!(
        "http_request",
        method = %method,
        path = %path,
        tenant = tracing::field::Empty,
    );
    let res = tracing::Instrument::instrument(next.run(req), span).await;
    let status = res.status().as_u16().to_string();
    let labels = [("method", method), ("path", path), ("status", status)];
    metrics::counter!("vs_api_http_requests_total", &labels).increment(1);
    metrics::histogram!("vs_api_http_request_duration_seconds", &labels).record(start.elapsed().as_secs_f64());
    res
}

async fn metrics_handler(State(s): State<AppState>, _: Admin) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], s.metrics.render()).into_response()
}

// ───────────── idempotency ─────────────

/// Replays a stored `{status, body}` response.
fn replay(stored: Value) -> Response {
    let status = stored.get("status").and_then(Value::as_u64).and_then(|c| StatusCode::from_u16(c as u16).ok()).unwrap_or(StatusCode::OK);
    let body = stored.get("body").cloned().unwrap_or(Value::Null);
    (status, [("idempotent-replayed", "true")], Json(body)).into_response()
}

fn idempotency_key(headers: &HeaderMap, route: &str) -> Result<Option<String>, ApiError> {
    match headers.get("idempotency-key") {
        None => Ok(None),
        Some(v) => {
            let k = v.to_str().map_err(|_| (StatusCode::BAD_REQUEST, "Idempotency-Key must be ASCII".to_string()))?.trim();
            if k.is_empty() || k.len() > 255 {
                return Err((StatusCode::BAD_REQUEST, "Idempotency-Key must be 1..255 characters".into()));
            }
            Ok(Some(format!("{route}:{k}")))
        }
    }
}

/// Runs `f` under the request's `Idempotency-Key` (if any): a stored response is replayed,
/// a new successful response is stored for 24 h. Two concurrent first requests with the
/// same key are not serialised (both run; the later store is ignored).
async fn idempotent<T, F>(s: &AppState, tenant: &TenantId, headers: &HeaderMap, route: &str, f: F) -> Result<Response, ApiError>
where
    T: Serialize,
    F: std::future::Future<Output = Result<(StatusCode, T), ApiError>>,
{
    let key = idempotency_key(headers, route)?;
    if let Some(k) = &key {
        if let Some(stored) = s.ops.get_idempotent(tenant, k).await.map_err(internal)? {
            return Ok(replay(stored));
        }
    }
    let (status, body) = f.await?;
    let body = serde_json::to_value(&body).map_err(internal)?;
    if let Some(k) = &key {
        if let Err(e) = s.ops.put_idempotent(tenant, k, &json!({ "status": status.as_u16(), "body": body })).await {
            tracing::warn!(error = %e, "could not store idempotency key");
        }
    }
    Ok((status, Json(body)).into_response())
}

// ───────────── documents ─────────────

/// Keys a tenant may reference as `source_uri`.
fn tenant_may_read(key: &str, tenant: &TenantId) -> bool {
    let t = tenant.as_str();
    !key.contains("..") && (key.starts_with(&format!("originals/{t}/")) || key.starts_with(&format!("inbox/{t}/")))
}

/// 429 for interactive requests while the interactive ingest lane is backed up. Batch
/// requests always queue.
async fn check_backpressure(s: &AppState, priority: Priority) -> Result<(), Response> {
    if priority != Priority::Interactive {
        return Ok(());
    }
    // The depth is local now: the documents this process has accepted and not finished,
    // rather than a queue every copy can see. Admission is therefore per copy — which is
    // also where the work will run, so it is the number that matters.
    let n = s.dispatch.in_flight();
    if n > s.api.interactive_max_pending {
        metrics::counter!("vs_api_backpressure_total").increment(1);
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_S))],
            format!("{n} documents in flight (limit {}); retry later or use priority=batch", s.api.interactive_max_pending),
        )
            .into_response());
    }
    Ok(())
}

/// Creates the job row and publishes the ingest task.
async fn enqueue(s: &AppState, spec: JobSpec, document_hk: Option<HashKey>) -> Result<Accepted, ApiError> {
    let (job_id, run_id) = (uuid::Uuid::now_v7(), RunId::new());
    s.ops.create_job_for_document(job_id, None, run_id, &spec, document_hk.as_ref()).await.map_err(internal)?;
    if let Err(e) = s.dispatch.dispatch(job_id, run_id, spec.clone()).await {
        // The job row exists, so it must not be left looking accepted.
        let _ = s.ops.set_state(job_id, JobState::Failed, Some("could not start processing")).await;
        tracing::error!(%job_id, error = %e, "dispatch failed");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "cannot accept work, retry later".into()));
    }
    metrics::counter!("vs_api_jobs_created_total", "priority" => vs_vault::enum_name(&spec.priority)).increment(1);
    Ok(Accepted { job_id, document_hk: document_hk.map(|h| h.to_string()), run_id: run_id.0, status_url: format!("/v1/jobs/{job_id}") })
}

/// `?skip_if_processed=true`: the tenant's current run of this document, unless it failed.
async fn existing_run(s: &AppState, tenant: &TenantId, document_hk: &HashKey) -> Result<Option<Accepted>, ApiError> {
    let Some(cur) = s.ops.current_run(tenant, document_hk).await.map_err(internal)? else { return Ok(None) };
    if cur.state == vs_vault::enum_name(&JobState::Failed) {
        return Ok(None);
    }
    Ok(Some(Accepted {
        job_id: cur.job_id,
        document_hk: Some(document_hk.to_string()),
        run_id: cur.run_id.0,
        status_url: format!("/v1/jobs/{}", cur.job_id),
    }))
}

fn spec_from(tenant: TenantId, owner: Owner, opts: &UploadOptions, source_uri: String) -> JobSpec {
    JobSpec {
        tenant,
        owner: Some(owner),
        language: opts.language.clone(),
        priority: opts.priority,
        profile: opts.profile,
        dag_list_version: opts.dag_list_version.clone(),
        source_uri,
        external_ref: opts.external_ref.clone(),
        doc_type: opts.doc_type,
    }
}

/// `POST /v1/documents`: multipart (`file` + `options`) or JSON (`source_uri`).
async fn create_document(
    State(s): State<AppState>,
    Caller(caller): Caller,
    Query(q): Query<DocumentQuery>,
    req: Request,
) -> Result<Response, Response> {
    let tenant = caller.tenant.clone();
    let headers = req.headers().clone();
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"));

    // Options (and the spooled file) first, so back-pressure can be checked before any job is created.
    let (opts, upload) = if is_multipart {
        let mp = Multipart::from_request(req, &s).await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
        let (opts, up) = read_multipart(&s, mp).await.map_err(IntoResponse::into_response)?;
        (opts, Some(up))
    } else {
        let Json(opts) = Json::<UploadOptions>::from_request(req, &s).await.map_err(|e| e.into_response())?;
        (opts, None)
    };
    check_backpressure(&s, opts.priority).await?;

    let s2 = s.clone();
    let tenant2 = tenant.clone();
    let owner = caller.owner();
    idempotent(&s, &tenant, &headers, "documents", async move {
        let s = s2;
        let tenant = tenant2;
        match upload {
            Some(up) => {
                let document_hk = HashKey::document(&tenant, &up.sha256);
                let key = keys::original(tenant.as_str(), &up.sha256);
                let result = async {
                    if !s.blob.exists(&key).await.map_err(internal)? {
                        let stream = file_stream(up.path.clone());
                        s.blob.put_stream(&key, stream, &up.content_type).await.map_err(internal)?;
                    }
                    Ok::<(), ApiError>(())
                }
                .await;
                let _ = tokio::fs::remove_file(&up.path).await;
                result?;
                if q.skip_if_processed {
                    if let Some(existing) = existing_run(&s, &tenant, &document_hk).await? {
                        return Ok((StatusCode::OK, existing));
                    }
                }
                let accepted = enqueue(&s, spec_from(tenant, owner.clone(), &opts, key), Some(document_hk)).await?;
                Ok((StatusCode::ACCEPTED, accepted))
            }
            None => {
                let uri = opts.source_uri.clone().ok_or((StatusCode::BAD_REQUEST, "source_uri required (or send multipart/form-data)".to_string()))?;
                let key = key_from_uri(&uri, &s.bucket).to_string();
                if !tenant_may_read(&key, &tenant) {
                    return Err((StatusCode::FORBIDDEN, format!("source_uri must be under originals/{0}/ or inbox/{0}/", tenant.as_str())));
                }
                if !s.blob.exists(&key).await.map_err(internal)? {
                    return Err((StatusCode::BAD_REQUEST, format!("object not found: {key}")));
                }
                // The hash key is known only after reading the object; done for
                // `skip_if_processed`, otherwise ingest computes it.
                let mut document_hk = None;
                if q.skip_if_processed {
                    let sha = hash_blob(&s, &key).await?;
                    let hk = HashKey::document(&tenant, &sha);
                    if let Some(existing) = existing_run(&s, &tenant, &hk).await? {
                        return Ok((StatusCode::OK, existing));
                    }
                    document_hk = Some(hk);
                }
                let accepted = enqueue(&s, spec_from(tenant, owner.clone(), &opts, key), document_hk).await?;
                Ok((StatusCode::ACCEPTED, accepted))
            }
        }
    })
    .await
    .map_err(IntoResponse::into_response)
}

/// A multipart upload spooled to disk while hashing.
struct Upload {
    path: PathBuf,
    sha256: String,
    size: u64,
    content_type: String,
}

/// Reads `options` and `file` parts. The file is hashed while being written to the
/// scratch dir (the blob key is its hash, so it cannot be streamed to the store directly).
async fn read_multipart(s: &AppState, mut mp: Multipart) -> Result<(UploadOptions, Upload), ApiError> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    let mut opts: Option<UploadOptions> = None;
    let mut upload: Option<Upload> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| bad(e.to_string()))? {
        match field.name() {
            Some("options") => {
                let text = field.text().await.map_err(|e| bad(e.to_string()))?;
                opts = Some(serde_json::from_str(&text).map_err(|e| bad(format!("options: {e}")))?);
            }
            Some("file") => {
                if upload.is_some() {
                    return Err(bad("only one file part allowed".into()));
                }
                let content_type = field.content_type().unwrap_or("application/octet-stream").to_string();
                let dir = s.api.scratch_dir.clone().unwrap_or_else(std::env::temp_dir);
                let path = dir.join(format!("vs-upload-{}", uuid::Uuid::now_v7()));
                let mut file = tokio::fs::File::create(&path).await.map_err(internal)?;
                let mut hasher = Sha256::new();
                let mut size: u64 = 0;
                let mut field = field;
                let res: Result<(), ApiError> = async {
                    while let Some(chunk) = field.chunk().await.map_err(|e| bad(e.to_string()))? {
                        size += chunk.len() as u64;
                        if size > s.api.max_upload_bytes {
                            return Err((StatusCode::PAYLOAD_TOO_LARGE, format!("file exceeds max_upload_bytes ({})", s.api.max_upload_bytes)));
                        }
                        hasher.update(&chunk);
                        file.write_all(&chunk).await.map_err(internal)?;
                    }
                    file.flush().await.map_err(internal)?;
                    Ok(())
                }
                .await;
                if let Err(e) = res {
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(e);
                }
                upload = Some(Upload { path, sha256: hex::encode(hasher.finalize()), size, content_type });
            }
            _ => {} // ignore unknown parts
        }
    }
    let cleanup = |u: &Option<Upload>| {
        if let Some(u) = u {
            let p = u.path.clone();
            tokio::spawn(async move {
                let _ = tokio::fs::remove_file(p).await;
            });
        }
    };
    let Some(opts) = opts else {
        cleanup(&upload);
        return Err(bad("missing `options` part".into()));
    };
    if opts.source_uri.is_some() {
        cleanup(&upload);
        return Err(bad("source_uri is not allowed with a file part".into()));
    }
    let Some(up) = upload else { return Err(bad("missing `file` part".into())) };
    if up.size == 0 {
        cleanup(&Some(up));
        return Err(bad("empty file".into()));
    }
    Ok((opts, up))
}

/// Streams a spooled file in 1 MiB chunks.
fn file_stream(path: PathBuf) -> BoxStream<'static, Result<Bytes, BlobError>> {
    use tokio::io::AsyncReadExt;
    let open = async move { tokio::fs::File::open(&path).await.map_err(|e| BlobError::Store(e.to_string())) };
    futures::stream::once(open)
        .flat_map(|file| match file {
            Err(e) => futures::stream::once(async { Err(e) }).boxed(),
            Ok(file) => futures::stream::unfold(Some(file), |file| async move {
                let mut file = file?;
                let mut buf = vec![0u8; 1024 * 1024];
                match file.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        Some((Ok(Bytes::from(buf)), Some(file)))
                    }
                    Err(e) => Some((Err(BlobError::Store(e.to_string())), None)),
                }
            })
            .boxed(),
        })
        .boxed()
}

/// SHA-256 of an object in the bucket, streamed.
async fn hash_blob(s: &AppState, key: &str) -> Result<String, ApiError> {
    let mut stream = s.blob.get_stream(key).await.map_err(internal)?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.try_next().await.map_err(internal)? {
        hasher.update(&chunk);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ───────────── jobs ─────────────

#[derive(Serialize)]
struct JobStatus {
    job_id: uuid::Uuid,
    state: String,
    document_hk: Option<String>,
    run_id: uuid::Uuid,
    pages_total: Option<i32>,
    pages_done: i32,
    pages_failed: i32,
    error: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    timings: Option<vs_vault::ops::JobTimings>,
}

async fn get_job(State(s): State<AppState>, Tenant(tenant): Tenant, Path(job_id): Path<uuid::Uuid>) -> Result<Json<JobStatus>, ApiError> {
    let j = s.ops.get_job(&tenant, job_id).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "job not found".into()))?;
    let timings = s.ops.job_timings(&tenant, job_id).await.map_err(internal)?;
    Ok(Json(JobStatus {
        job_id: j.job_id,
        state: j.state,
        document_hk: j.document_hk,
        run_id: j.run_id.0,
        pages_total: j.pages_total,
        pages_done: j.pages_done,
        pages_failed: j.pages_failed,
        error: j.error,
        created_at: j.created_at,
        updated_at: j.updated_at,
        timings,
    }))
}

/// Postgres and blob store reachable (the bus connection is checked at startup and
/// reconnects on its own).
async fn readyz(State(s): State<AppState>) -> (StatusCode, &'static str) {
    let pg = sqlx::query("SELECT 1").execute(&s.ops.pool).await.is_ok();
    let blob = s.blob.exists("readyz-probe").await.is_ok();
    match (pg, blob) {
        (true, true) => (StatusCode::OK, "ready"),
        (false, _) => (StatusCode::SERVICE_UNAVAILABLE, "postgres unreachable"),
        (_, false) => (StatusCode::SERVICE_UNAVAILABLE, "blob store unreachable"),
    }
}

// ───────────── batches ─────────────

#[derive(Debug, Deserialize)]
struct BatchOptions {
    /// Applies to every document in the batch unless one overrides it.
    doc_type: Option<vs_core::DocType>,
    /// Default for documents without their own `language`.
    language: Option<LanguageTag>,
    #[serde(default)]
    priority: Priority,
    #[serde(default)]
    profile: Profile,
    dag_list_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchDocument {
    source_uri: String,
    external_ref: Option<String>,
    language: Option<LanguageTag>,
    /// Overrides the batch's `doc_type`, for a mixed batch.
    doc_type: Option<vs_core::DocType>,
}

#[derive(Debug, Deserialize)]
struct BatchRequest {
    options: BatchOptions,
    documents: Vec<BatchDocument>,
}

#[derive(Serialize)]
struct BatchAccepted {
    batch_id: uuid::Uuid,
    job_ids: Vec<uuid::Uuid>,
    /// Jobs whose ingest task could not be published (marked `failed`).
    failed_job_ids: Vec<uuid::Uuid>,
    status_url: String,
}

async fn create_batch(
    State(s): State<AppState>,
    Caller(caller): Caller,
    headers: HeaderMap,
    Json(req): Json<BatchRequest>,
) -> Result<Response, Response> {
    let tenant = caller.tenant.clone();
    if req.documents.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "documents must not be empty").into_response());
    }
    if req.documents.len() > MAX_BATCH_DOCUMENTS {
        return Err((StatusCode::BAD_REQUEST, format!("at most {MAX_BATCH_DOCUMENTS} documents per batch")).into_response());
    }
    // Validate every document before touching any state.
    let mut specs = Vec::with_capacity(req.documents.len());
    for (i, d) in req.documents.iter().enumerate() {
        let language = d
            .language
            .clone()
            .or_else(|| req.options.language.clone())
            .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("documents[{i}]: language required (per document or in options)")).into_response())?;
        let key = key_from_uri(&d.source_uri, &s.bucket).to_string();
        if !tenant_may_read(&key, &tenant) {
            return Err((StatusCode::FORBIDDEN, format!("documents[{i}]: source_uri must be under originals/{0}/ or inbox/{0}/", tenant.as_str()))
                .into_response());
        }
        specs.push(JobSpec {
            tenant: tenant.clone(),
            owner: Some(caller.owner()),
            language,
            priority: req.options.priority,
            profile: req.options.profile,
            dag_list_version: req.options.dag_list_version.clone(),
            source_uri: key,
            external_ref: d.external_ref.clone(),
            doc_type: d.doc_type.or(req.options.doc_type),
        });
    }
    let keys: Vec<String> = specs.iter().map(|sp| sp.source_uri.clone()).collect();
    let missing: Vec<String> = futures::stream::iter(keys)
        .map(|key| {
            let blob = s.blob.clone();
            async move { (blob.exists(&key).await, key) }
        })
        .buffer_unordered(BATCH_CONCURRENCY)
        .filter_map(|(r, key)| async move {
            match r {
                Ok(true) => None,
                Ok(false) => Some(Ok(key)),
                Err(e) => Some(Err(e)),
            }
        })
        .try_collect()
        .await
        .map_err(|e| internal(e).into_response())?;
    if !missing.is_empty() {
        let shown: Vec<&str> = missing.iter().take(10).map(String::as_str).collect();
        return Err((StatusCode::BAD_REQUEST, format!("{} objects not found, e.g. {}", missing.len(), shown.join(", "))).into_response());
    }
    check_backpressure(&s, req.options.priority).await?;

    let s2 = s.clone();
    let tenant2 = tenant.clone();
    idempotent(&s, &tenant, &headers, "batches", async move {
        let (s, tenant) = (s2, tenant2);
        let batch_id = uuid::Uuid::now_v7();
        let jobs: Vec<NewJob> =
            specs.into_iter().map(|spec| NewJob { job_id: uuid::Uuid::now_v7(), run_id: RunId::new(), spec, document_hk: None }).collect();
        s.ops.create_batch(batch_id, &tenant, jobs.len() as u32).await.map_err(internal)?;
        s.ops.create_jobs(Some(batch_id), &jobs).await.map_err(internal)?;

        // Each document is dispatched on its own: one that cannot be accepted is
        // reported in `failed_job_ids` and marked failed, and the rest still run.
        let failed: Vec<uuid::Uuid> = futures::stream::iter(jobs.clone())
            .map(|j| {
                let (dispatch, ops) = (s.dispatch.clone(), s.ops.clone());
                async move {
                    match dispatch.dispatch(j.job_id, j.run_id, j.spec.clone()).await {
                        Ok(()) => None,
                        Err(e) => {
                            tracing::error!(job_id = %j.job_id, error = %e, "dispatch failed");
                            let _ = ops.set_state(j.job_id, JobState::Failed, Some("could not start processing")).await;
                            Some(j.job_id)
                        }
                    }
                }
            })
            .buffer_unordered(BATCH_CONCURRENCY)
            .filter_map(|x| async move { x })
            .collect()
            .await;
        let n_ok = jobs.len() - failed.len();
        metrics::counter!("vs_api_jobs_created_total", "priority" => vs_vault::enum_name(&req.options.priority)).increment(n_ok as u64);
        Ok((
            StatusCode::ACCEPTED,
            BatchAccepted {
                batch_id,
                job_ids: jobs.iter().map(|j| j.job_id).collect(),
                failed_job_ids: failed,
                status_url: format!("/v1/batches/{batch_id}"),
            },
        ))
    })
    .await
    .map_err(IntoResponse::into_response)
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    #[serde(default)]
    page: u32,
    #[serde(default = "default_page_size")]
    page_size: u32,
}

fn default_page_size() -> u32 {
    100
}

#[derive(Serialize)]
struct BatchStatus {
    batch_id: uuid::Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
    documents: i32,
    total_jobs: i64,
    counts: BTreeMap<String, i64>,
    page: u32,
    page_size: u32,
    jobs: Vec<BatchJob>,
}

#[derive(Serialize)]
struct BatchJob {
    job_id: uuid::Uuid,
    state: String,
    document_hk: Option<String>,
    run_id: uuid::Uuid,
    external_ref: Option<String>,
    pages_total: Option<i32>,
    pages_done: i32,
    pages_failed: i32,
    error: Option<String>,
}

async fn get_batch(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    Path(batch_id): Path<uuid::Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Json<BatchStatus>, ApiError> {
    let b = s
        .ops
        .get_batch(&tenant, batch_id, q.page, q.page_size)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "batch not found".into()))?;
    Ok(Json(BatchStatus {
        batch_id: b.batch_id,
        created_at: b.created_at,
        documents: b.documents,
        total_jobs: b.total_jobs,
        counts: b.counts,
        page: b.page,
        page_size: b.page_size,
        jobs: b
            .jobs
            .into_iter()
            .map(|j| BatchJob {
                job_id: j.job_id,
                state: j.state,
                document_hk: j.document_hk,
                run_id: j.run_id.0,
                external_ref: j.spec.external_ref,
                pages_total: j.pages_total,
                pages_done: j.pages_done,
                pages_failed: j.pages_failed,
                error: j.error,
            })
            .collect(),
    }))
}

// ───────────── document results ─────────────

#[derive(Debug, Default, Deserialize)]
struct RunQuery {
    run_id: Option<uuid::Uuid>,
}

fn parse_hk(hk: &str) -> Result<HashKey, ApiError> {
    vs_vault::hash_key_from_hex(hk).ok_or((StatusCode::BAD_REQUEST, "document_hk must be 64 hex characters".into()))
}

async fn get_document(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    Path(hk): Path<String>,
    Query(q): Query<RunQuery>,
) -> Result<Json<vs_vault::ops::DocumentSummary>, ApiError> {
    let hk = parse_hk(&hk)?;
    s.ops
        .document_summary(&tenant, &hk, q.run_id.map(RunId))
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "document or run not found".into()))
}

/// The stored `PageResult` JSON, as written by the page worker.
async fn get_page(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    Path((hk, page_no)): Path<(String, u32)>,
    Query(q): Query<RunQuery>,
) -> Result<Response, ApiError> {
    let hk = parse_hk(&hk)?;
    let run = s
        .ops
        .document_run(&tenant, &hk, q.run_id.map(RunId))
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "document or run not found".into()))?;
    let key = keys::page_result(hk.as_str(), &run.to_string(), page_no);
    match s.blob.get_stream(&key).await {
        Ok(stream) => {
            let body = Body::from_stream(stream.map_err(std::io::Error::other));
            Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
        }
        Err(BlobError::NotFound(_)) => Err((StatusCode::NOT_FOUND, "page result not available (page not processed yet or failed)".into())),
        Err(e) => Err(internal(e)),
    }
}

// ───────────── DAG lists (admin) ─────────────

#[derive(Debug, Default, Deserialize)]
struct ScopeQuery {
    /// `true`: the list is global (all tenants without their own); default: the calling tenant's.
    #[serde(default)]
    global: bool,
}

async fn list_dag_lists(State(s): State<AppState>, Tenant(tenant): Tenant, _: Admin) -> Result<Json<Vec<vs_vault::ops::DagListRow>>, ApiError> {
    Ok(Json(s.ops.dag_lists_list(&tenant).await.map_err(internal)?))
}

/// Validates the candidate against the production step registry and stores it as `candidate`.
async fn upload_dag_list(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    _: Admin,
    Query(q): Query<ScopeQuery>,
    Json(list): Json<vs_router::DagList>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let steps = s.steps.as_ref().ok_or((StatusCode::SERVICE_UNAVAILABLE, "step registry unavailable (model registry failed to load)".to_string()))?;
    let version = list.version.trim().to_string();
    if version.is_empty() || version.len() > 128 {
        return Err((StatusCode::BAD_REQUEST, "version must be 1..128 characters".into()));
    }
    vs_router::Router::load(list.clone(), steps.as_ref()).map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid DAG list: {e}")))?;
    let scope = if q.global { None } else { Some(&tenant) };
    let created_by = list.created_by.clone();
    let inserted = s.ops.dag_list_insert_candidate(&version, scope, &list, created_by.as_deref()).await.map_err(internal)?;
    if !inserted {
        return Err((StatusCode::CONFLICT, format!("DAG list version {version:?} exists")));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "version": version, "status": "candidate", "tenant_id": scope.map(|t| t.as_str()), "dags": list.dags.len() })),
    ))
}

async fn activate_dag_list(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    _: Admin,
    Path(version): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<Value>, ApiError> {
    let scope = if q.global { None } else { Some(&tenant) };
    let ok = s.ops.dag_list_activate(&version, scope).await.map_err(internal)?;
    if !ok {
        return Err((StatusCode::NOT_FOUND, "DAG list not found in this scope (use ?global=true for a global list)".into()));
    }
    Ok(Json(json!({ "version": version, "status": "active", "tenant_id": scope.map(|t| t.as_str()) })))
}

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_prefixes() {
        let t = TenantId("t1".into());
        assert!(tenant_may_read("originals/t1/abc", &t));
        assert!(tenant_may_read("inbox/t1/x.pdf", &t));
        assert!(!tenant_may_read("originals/t2/abc", &t));
        assert!(!tenant_may_read("originals/t1/../t2/abc", &t));
    }

    #[test]
    fn idempotency_keys_are_scoped_per_route() {
        let mut h = HeaderMap::new();
        h.insert("idempotency-key", HeaderValue::from_static(" k1 "));
        assert_eq!(idempotency_key(&h, "documents").unwrap().as_deref(), Some("documents:k1"));
        assert!(idempotency_key(&HeaderMap::new(), "documents").unwrap().is_none());
        h.insert("idempotency-key", HeaderValue::from_static(""));
        assert!(idempotency_key(&h, "documents").is_err());
    }

    #[test]
    fn replay_keeps_status() {
        let r = replay(json!({ "status": 202, "body": { "a": 1 } }));
        assert_eq!(r.status(), StatusCode::ACCEPTED);
    }

    #[test]
    fn hash_key_path_parsing() {
        assert!(parse_hk("zz").is_err());
        let hk = HashKey::of(&["x"]);
        assert_eq!(parse_hk(hk.as_str()).unwrap(), hk);
    }
}
