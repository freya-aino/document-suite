//! Public HTTP API. See `SPEC.md`.

use std::str::FromStr;
use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, Path, State},
    http::{request::Parts, StatusCode},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use vs_core::{JobSpec, LanguageTag, Priority, Profile, RunId, Task, TenantId};
use vs_infra::blob::{key_from_uri, BlobStore};
use vs_infra::queue::{Lane, NatsBus, Role, TaskBus};
use vs_infra::ObjectBlobStore;
use vs_vault::ops::Ops;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "VS_CONFIG", default_value = "config/worker.toml")]
    config: std::path::PathBuf,
    #[arg(long, env = "VS_API_ADDR", default_value = "0.0.0.0:8080")]
    addr: String,
}

#[derive(Clone)]
struct AppState {
    ops: Arc<Ops>,
    bus: Arc<dyn TaskBus>,
    blob: Arc<dyn BlobStore>,
    bucket: String,
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

#[derive(Debug, Serialize)]
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = vs_infra::Config::load(&args.config)?;
    vs_infra::telemetry::init(&cfg.telemetry);

    let mut pg = PgConnectOptions::from_str(&cfg.postgres.url)?;
    if let Some(pw) = &cfg.postgres.password {
        pg = pg.password(pw);
    }
    let pool = PgPoolOptions::new().max_connections(cfg.postgres.max_connections).connect_with(pg).await?;
    vs_vault::MIGRATOR.run(&pool).await?;
    let nats = NatsBus::connect(&cfg.bus.nats_url).await?;
    nats.ensure_topology().await?;
    let state = AppState {
        ops: Arc::new(Ops { pool }),
        bus: Arc::new(nats),
        blob: Arc::new(ObjectBlobStore::s3(&cfg.blob)?),
        bucket: cfg.blob.bucket.clone(),
    };

    let app = Router::new()
        .route("/v1/documents", post(create_document))
        .route("/v1/batches", post(create_batch))
        .route("/v1/jobs/{job_id}", get(get_job))
        .route("/v1/batches/{batch_id}", get(get_batch))
        .route("/v1/documents/{document_hk}", get(get_document))
        .route("/v1/documents/{document_hk}/pages/{page_no}", get(get_page))
        .route("/v1/dag-lists", get(list_dag_lists).post(upload_dag_list))
        .route("/v1/dag-lists/{version}/activate", post(activate_dag_list))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    tracing::info!(addr = %args.addr, "vs-api listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Keys a tenant may reference as `source_uri`.
fn tenant_may_read(key: &str, tenant: &TenantId) -> bool {
    let t = tenant.as_str();
    !key.contains("..") && (key.starts_with(&format!("originals/{t}/")) || key.starts_with(&format!("inbox/{t}/")))
}

/// JSON variant: the original is already in the bucket under the tenant's prefix.
/// Multipart streaming upload is still to do (SPEC.md).
async fn create_document(
    State(s): State<AppState>,
    Tenant(tenant): Tenant,
    Json(opts): Json<UploadOptions>,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    let uri = opts
        .source_uri
        .ok_or((StatusCode::BAD_REQUEST, "source_uri required (multipart upload not implemented yet)".into()))?;
    let key = key_from_uri(&uri, &s.bucket).to_string();
    if !tenant_may_read(&key, &tenant) {
        return Err((StatusCode::FORBIDDEN, format!("source_uri must be under originals/{0}/ or inbox/{0}/", tenant.as_str())));
    }
    if !s.blob.exists(&key).await.map_err(internal)? {
        return Err((StatusCode::BAD_REQUEST, format!("object not found: {key}")));
    }

    let (job_id, run_id) = (uuid::Uuid::now_v7(), RunId::new());
    let spec = JobSpec {
        tenant,
        language: opts.language,
        priority: opts.priority,
        profile: opts.profile,
        dag_list_version: opts.dag_list_version,
        source_uri: key,
        external_ref: opts.external_ref,
    };
    s.ops.create_job(job_id, None, run_id, &spec).await.map_err(internal)?;
    let task = Task::Ingest { job_id, run_id, spec: spec.clone() };
    if let Err(e) = s.bus.publish(Role::Ingest, Lane::from(spec.priority), &task, &format!("{job_id}:ingest")).await {
        let _ = s.ops.set_state(job_id, vs_core::JobState::Failed, Some("could not enqueue ingest")).await;
        tracing::error!(%job_id, error = %e, "enqueue failed");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "task bus unavailable, retry later".into()));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted { job_id, document_hk: None, run_id: run_id.0, status_url: format!("/v1/jobs/{job_id}") }),
    ))
}

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
}

async fn get_job(State(s): State<AppState>, Tenant(tenant): Tenant, Path(job_id): Path<uuid::Uuid>) -> Result<Json<JobStatus>, ApiError> {
    let j = s.ops.get_job(&tenant, job_id).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "job not found".into()))?;
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

async fn create_batch(State(_s): State<AppState>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "create_batch")
}


async fn get_batch(State(_s): State<AppState>, Path(_batch_id): Path<uuid::Uuid>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "get_batch")
}

async fn get_document(State(_s): State<AppState>, Path(_hk): Path<String>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "get_document")
}

async fn get_page(State(_s): State<AppState>, Path((_hk, _page)): Path<(String, u32)>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "get_page")
}

async fn list_dag_lists(State(_s): State<AppState>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "list_dag_lists")
}

async fn upload_dag_list(State(_s): State<AppState>, Json(_list): Json<vs_router::DagList>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "upload_dag_list: validate against production StepRegistry, insert as candidate")
}

async fn activate_dag_list(State(_s): State<AppState>, Path(_version): Path<String>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "activate_dag_list")
}

