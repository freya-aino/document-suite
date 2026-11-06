//! Public HTTP API. See `SPEC.md`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use vs_core::{LanguageTag, Priority, Profile};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "VS_CONFIG", default_value = "config/worker.toml")]
    config: std::path::PathBuf,
    #[arg(long, env = "VS_API_ADDR", default_value = "0.0.0.0:8080")]
    addr: String,
}

#[derive(Clone)]
struct AppState {
    // pool: sqlx::PgPool,
    // bus: Arc<dyn vs_infra::TaskBus>,
    // blob: Arc<dyn vs_infra::BlobStore>,
    _cfg: Arc<vs_infra::Config>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = vs_infra::Config::load(&args.config)?;
    vs_infra::telemetry::init(&cfg.telemetry);
    let state = AppState { _cfg: Arc::new(cfg) };

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

/// JSON variant only in the scaffold; multipart streaming upload is listed in SPEC.md.
async fn create_document(State(_s): State<AppState>, Json(opts): Json<UploadOptions>) -> Result<(StatusCode, Json<Accepted>), (StatusCode, String)> {
    let Some(_uri) = opts.source_uri.as_ref() else {
        return Err((StatusCode::BAD_REQUEST, "source_uri required (multipart upload not wired yet)".into()));
    };
    let _ = (&opts.language, opts.priority, opts.profile, &opts.dag_list_version, &opts.external_ref);
    // 1. verify object exists, read sha256 (or compute)  2. insert ops.jobs  3. publish Task::Ingest
    Err((StatusCode::NOT_IMPLEMENTED, "create_document".into()))
}

async fn create_batch(State(_s): State<AppState>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "create_batch")
}

async fn get_job(State(_s): State<AppState>, Path(_job_id): Path<uuid::Uuid>) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "get_job")
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

async fn readyz(State(_s): State<AppState>) -> (StatusCode, &'static str) {
    (StatusCode::SERVICE_UNAVAILABLE, "readiness checks not wired")
}
