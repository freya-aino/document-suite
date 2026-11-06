//! Public HTTP API. See `SPEC.md`.

// Handlers return `Response` in the error position for headers like `Retry-After`.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, FromRequestParts, MatchedPath, Multipart, Path, Query, Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use clap::Parser;
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::io::AsyncWriteExt;

use vs_core::{HashKey, JobSpec, JobState, LanguageTag, Priority, Profile, RunId, Task, TenantId};
use vs_dag::StepRegistry;
use vs_infra::blob::{key_from_uri, keys, BlobError, BlobStore};
use vs_infra::config::ApiConfig;
use vs_infra::queue::{Lane, NatsBus, Role, TaskBus};
use vs_infra::ObjectBlobStore;
use vs_vault::ops::{NewJob, Ops};

/// Documents per `POST /v1/batches`.
const MAX_BATCH_DOCUMENTS: usize = 10_000;
/// Concurrent blob HEADs / publishes while handling a batch.
const BATCH_CONCURRENCY: usize = 32;
/// Seconds clients should wait after a 429.
const RETRY_AFTER_S: &str = "30";

#[derive(Parser)]
struct Args {
    #[arg(long, env = "VS_CONFIG", default_value = "config/worker.toml")]
    config: std::path::PathBuf,
    #[arg(long, env = "VS_API_ADDR", default_value = "0.0.0.0:8080")]
    addr: String,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Apply database migrations (deploy step; needs the schema owner's credentials,
    /// not the runtime role's).
    Migrate {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
    },
}

#[derive(Clone)]
struct AppState {
    ops: Arc<Ops>,
    bus: Arc<dyn TaskBus>,
    blob: Arc<dyn BlobStore>,
    bucket: String,
    api: ApiConfig,
    /// Production step registry for DAG-list validation; `None` if the model registry
    /// could not be loaded at startup (DAG-list uploads then get 503).
    steps: Option<Arc<StepRegistry>>,
    metrics: PrometheusHandle,
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

/// Calling tenant.
///
/// PLACEHOLDER until the auth decision (mTLS or JWT from the service-api, SPEC.md):
/// read from the `X-VS-Tenant` header, which any caller can set. Do not expose the API
/// beyond the trusted network in this state.
struct Tenant(TenantId);

impl<S: Send + Sync> FromRequestParts<S> for Tenant {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let t = parts
            .headers
            .get("x-vs-tenant")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .ok_or((StatusCode::UNAUTHORIZED, "missing or invalid X-VS-Tenant".to_string()))?;
        Ok(Tenant(TenantId(t.to_string())))
    }
}

/// Admin routes (DAG lists).
///
/// PLACEHOLDER like `Tenant`: the `X-VS-Admin: 1` header, which any caller can set, until
/// roles come from the auth token.
struct Admin;

impl<S: Send + Sync> FromRequestParts<S> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        match parts.headers.get("x-vs-admin").and_then(|v| v.to_str().ok()).map(str::trim) {
            Some("1") => Ok(Admin),
            _ => Err((StatusCode::FORBIDDEN, "admin route: X-VS-Admin: 1 required".into())),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(Cmd::Migrate { database_url }) = &args.cmd {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        vs_vault::MIGRATOR.run(&pool).await?;
        vs_vault::check_migrations(&pool).await?;
        println!("migrations applied");
        return Ok(());
    }
    let cfg = vs_infra::Config::load(&args.config)?;
    vs_infra::telemetry::init(&cfg.telemetry);
    let metrics = PrometheusBuilder::new().install_recorder()?;

    let mut pg = PgConnectOptions::from_str(&cfg.postgres.url)?;
    if let Some(pw) = &cfg.postgres.password {
        pg = pg.password(pw);
    }
    let pool = PgPoolOptions::new().max_connections(cfg.postgres.max_connections).connect_with(pg).await?;
    vs_vault::check_migrations(&pool).await?;
    let nats = NatsBus::connect(&cfg.bus.nats_url).await?;
    nats.ensure_topology().await?;

    let steps = match vs_models::ModelRegistry::load(&cfg.models.registry) {
        Ok(reg) => Some(Arc::new(vs_steps::production_registry(Arc::new(vs_models::OpenAiClient::new(reg))))),
        Err(e) => {
            tracing::error!(path = %cfg.models.registry.display(), error = %e, "model registry not loaded; DAG-list uploads disabled");
            None
        }
    };

    let state = AppState {
        ops: Arc::new(Ops { pool }),
        bus: Arc::new(nats),
        blob: Arc::new(ObjectBlobStore::s3(&cfg.blob)?),
        bucket: cfg.blob.bucket.clone(),
        api: cfg.api.clone(),
        steps,
        metrics,
    };
    let app = router(state.clone());

    // Expired idempotency keys.
    let ops = state.ops.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = ops.purge_idempotency_keys().await {
                tracing::warn!(error = %e, "idempotency purge failed");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    tracing::info!(addr = %args.addr, "vs-api listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    // Body limit for uploads: the handler enforces `max_upload_bytes` on the file part
    // itself; the slack covers the other multipart parts.
    let upload_limit = usize::try_from(state.api.max_upload_bytes.saturating_add(1024 * 1024)).unwrap_or(usize::MAX);
    Router::new()
        .route("/v1/documents", post(create_document).layer(DefaultBodyLimit::max(upload_limit)))
        .route("/v1/batches", post(create_batch).layer(DefaultBodyLimit::max(16 * 1024 * 1024)))
        .route("/v1/jobs/{job_id}", get(get_job))
        .route("/v1/batches/{batch_id}", get(get_batch))
        .route("/v1/documents/{document_hk}", get(get_document))
        .route("/v1/documents/{document_hk}/pages/{page_no}", get(get_page))
        .route("/v1/dag-lists", get(list_dag_lists).post(upload_dag_list))
        .route("/v1/dag-lists/{version}/activate", post(activate_dag_list))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn(record_metrics))
        .with_state(state)
}

// ───────────── metrics ─────────────

async fn record_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req.extensions().get::<MatchedPath>().map(|p| p.as_str().to_string()).unwrap_or_else(|| "unmatched".into());
    let res = next.run(req).await;
    let status = res.status().as_u16().to_string();
    let labels = [("method", method), ("path", path), ("status", status)];
    metrics::counter!("vs_api_http_requests_total", &labels).increment(1);
    metrics::histogram!("vs_api_http_request_duration_seconds", &labels).record(start.elapsed().as_secs_f64());
    res
}

async fn metrics_handler(State(s): State<AppState>) -> Response {
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
    match s.bus.pending(Role::Ingest, Lane::Interactive).await {
        Ok(n) if n > s.api.interactive_max_pending => {
            metrics::counter!("vs_api_backpressure_total").increment(1);
            Err((
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_S))],
                format!("interactive lane has {n} pending tasks (limit {}); retry later or use priority=batch", s.api.interactive_max_pending),
            )
                .into_response())
        }
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read lane backlog; accepting");
            Ok(())
        }
    }
}

/// Creates the job row and publishes the ingest task.
async fn enqueue(s: &AppState, spec: JobSpec, document_hk: Option<HashKey>) -> Result<Accepted, ApiError> {
    let (job_id, run_id) = (uuid::Uuid::now_v7(), RunId::new());
    s.ops.create_job_for_document(job_id, None, run_id, &spec, document_hk.as_ref()).await.map_err(internal)?;
    let task = Task::Ingest { job_id, run_id, spec: spec.clone() };
    if let Err(e) = s.bus.publish(Role::Ingest, Lane::from(spec.priority), &task, &format!("{job_id}:ingest")).await {
        let _ = s.ops.set_state(job_id, JobState::Failed, Some("could not enqueue ingest")).await;
        tracing::error!(%job_id, error = %e, "enqueue failed");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "task bus unavailable, retry later".into()));
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

fn spec_from(tenant: TenantId, opts: &UploadOptions, source_uri: String) -> JobSpec {
    JobSpec {
        tenant,
        language: opts.language.clone(),
        priority: opts.priority,
        profile: opts.profile,
        dag_list_version: opts.dag_list_version.clone(),
        source_uri,
        external_ref: opts.external_ref.clone(),
    }
}

/// `POST /v1/documents`: multipart (`file` + `options`) or JSON (`source_uri`).
async fn create_document(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    Query(q): Query<DocumentQuery>,
    req: Request,
) -> Result<Response, Response> {
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
                let accepted = enqueue(&s, spec_from(tenant, &opts, key), Some(document_hk)).await?;
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
                let accepted = enqueue(&s, spec_from(tenant, &opts, key), document_hk).await?;
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
    Tenant(tenant): Tenant,
    headers: HeaderMap,
    Json(req): Json<BatchRequest>,
) -> Result<Response, Response> {
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
            language,
            priority: req.options.priority,
            profile: req.options.profile,
            dag_list_version: req.options.dag_list_version.clone(),
            source_uri: key,
            external_ref: d.external_ref.clone(),
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

        let tasks: Vec<(uuid::Uuid, Lane, Task)> = jobs
            .iter()
            .map(|j| (j.job_id, Lane::from(j.spec.priority), Task::Ingest { job_id: j.job_id, run_id: j.run_id, spec: j.spec.clone() }))
            .collect();
        let failed: Vec<uuid::Uuid> = futures::stream::iter(tasks)
            .map(|(job_id, lane, task)| {
                let (bus, ops) = (s.bus.clone(), s.ops.clone());
                async move {
                    match bus.publish(Role::Ingest, lane, &task, &format!("{job_id}:ingest")).await {
                        Ok(()) => None,
                        Err(e) => {
                            tracing::error!(%job_id, error = %e, "enqueue failed");
                            let _ = ops.set_state(job_id, JobState::Failed, Some("could not enqueue ingest")).await;
                            Some(job_id)
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
