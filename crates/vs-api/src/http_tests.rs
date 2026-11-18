//! The HTTP paths where a regression loses or mis-attributes work.
//!
//! Five of them had no test at all: idempotency replay, back-pressure, the batch path, the
//! `source_uri` prefix check and DAG-list activation. All five are the kind that fails
//! quietly — a replayed request that creates a second job, a tenant reading another
//! tenant's object, an activation that leaves two lists active — so they are asserted
//! against the real router, the real Postgres and a real body, not against the handlers'
//! internals.
//!
//! The blob store is `object_store`'s in-memory one and the bus is a fake whose backlog
//! the test sets, because the questions here are about the API's own decisions. The
//! tokens, though, are real: these requests carry a signed token for organization `t1`
//! and are verified by the same code a deployment runs, so the tenant they act on is the
//! one the token says. What authentication *refuses* is in `auth_tests.rs`. Needs
//! Postgres: `cargo test -p vs-api -- --ignored`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use vs_core::{JobSpec, RunId, TenantId};
use vs_infra::{BlobStore, ObjectBlobStore};
use vs_vault::ops::Ops;

use super::auth_tests::{admin_claims, claims, token, verifier, ORG, SUBJECT};
use super::{router, ApiConfig, AppState, Dispatch};

/// The organization in the test token, which is therefore the tenant.
const TENANT: &str = ORG;
const BUCKET: &str = "visual-suite";

/// Records what was dispatched and reports whatever in-flight count the test asked for.
pub(crate) struct FakeDispatch {
    dispatched: Mutex<Vec<(uuid::Uuid, RunId, JobSpec)>>,
    in_flight: u64,
}

impl FakeDispatch {
    /// A dispatcher carrying nothing, for the tests that never look at it.
    pub(crate) fn idle() -> Self {
        Self { dispatched: Mutex::new(Vec::new()), in_flight: 0 }
    }

    fn count(&self) -> usize {
        self.dispatched.lock().expect("lock").len()
    }
}

#[async_trait]
impl Dispatch for FakeDispatch {
    async fn dispatch(&self, job_id: uuid::Uuid, run_id: RunId, spec: JobSpec) -> anyhow::Result<()> {
        self.dispatched.lock().expect("lock").push((job_id, run_id, spec));
        Ok(())
    }

    fn in_flight(&self) -> u64 {
        self.in_flight
    }
}

/// `state(pool, backlog)` plus the objects the tests reference as `source_uri`.
async fn state(pool: PgPool, in_flight: u64) -> (AppState, Arc<FakeDispatch>) {
    let dispatch = Arc::new(FakeDispatch { dispatched: Mutex::new(Vec::new()), in_flight });
    let blob = Arc::new(ObjectBlobStore::new(Arc::new(object_store::memory::InMemory::new())));
    for key in ["originals/t1/a", "originals/t1/b", "inbox/t1/c", "originals/t2/secret"] {
        blob.put(key, bytes::Bytes::from_static(b"%PDF-1.7 fake"), "application/pdf").await.expect("seed blob");
    }
    let s = AppState {
        ops: Arc::new(Ops { pool }),
        auth: verifier(),
        dispatch: dispatch.clone(),
        blob,
        bucket: BUCKET.into(),
        api: ApiConfig { interactive_max_pending: 10, ..Default::default() },
        // DAG-list uploads are validated against the production registry, which needs a
        // model registry; the activation test does not go through the upload route.
        steps: None,
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder().handle(),
    };
    (s, dispatch)
}

async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
    let res = router(state.clone()).oneshot(req).await.expect("router");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.expect("body");
    let json = serde_json::from_slice(&body).unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&body) }));
    (status, json)
}

fn post(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", token(&claims())))
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn with_key(mut req: Request<Body>, key: &str) -> Request<Body> {
    req.headers_mut().insert("idempotency-key", key.parse().expect("header"));
    req
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_replayed_request_returns_the_first_answer_and_creates_no_second_job(pool: PgPool) {
    let (s, dispatch) = state(pool.clone(), 0).await;
    let body = json!({ "language": "en", "source_uri": "originals/t1/a" });

    let (first_status, first) = send(&s, with_key(post("/v1/documents", body.clone()), "k1")).await;
    assert_eq!(first_status, StatusCode::ACCEPTED, "{first}");
    let (replay_status, replayed) = send(&s, with_key(post("/v1/documents", body.clone()), "k1")).await;

    // The same status *and* the same body: a caller that lost the first response has to be
    // able to use the second one, which means the job id must be the first job's.
    assert_eq!(replay_status, StatusCode::ACCEPTED);
    assert_eq!(replayed, first, "a replay must return the stored response, not a new one");

    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM ops.jobs").fetch_one(&pool).await.expect("count jobs");
    assert_eq!(jobs, 1, "the replay created a second job");

    // The job records the person the token named, and the organization it is keyed
    // under. Enforcing read scoping within an organization is a later pass, but the only
    // place that identity exists is this request, so it is recorded now rather than
    // reconstructed later from nothing.
    let (tenant, subject): (String, Option<String>) =
        sqlx::query_as("SELECT spec->>'tenant', spec->'owner'->>'subject' FROM ops.jobs").fetch_one(&pool).await.expect("owner");
    assert_eq!(tenant, TENANT, "the tenant must be the token's organization");
    assert_eq!(subject.as_deref(), Some(SUBJECT), "the job does not say who asked for it");
    assert_eq!(dispatch.count(), 1, "the replay dispatched the document twice");

    // A different key is a different request, even with an identical body.
    let (status, other) = send(&s, with_key(post("/v1/documents", body.clone()), "k2")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_ne!(other["job_id"], first["job_id"]);

    // And the key is scoped per route, so the same key on `/v1/batches` is not a replay of
    // this one. Without that, a client reusing one key per upload would get a document
    // response back from a batch call.
    let batch = json!({ "options": { "language": "en" }, "documents": [{ "source_uri": "originals/t1/b" }] });
    let (status, batched) = send(&s, with_key(post("/v1/batches", batch), "k1")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{batched}");
    assert!(batched["batch_id"].is_string(), "a batch response, not the document replay: {batched}");
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn an_interactive_request_is_refused_while_the_lane_is_backed_up(pool: PgPool) {
    // Backlog above `interactive_max_pending` (10).
    let (s, dispatch) = state(pool.clone(), 11).await;

    let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "priority": "interactive", "source_uri": "originals/t1/a" }))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // A batch request always queues: back-pressure protects the interactive lane's
    // latency, and refusing batch work would just move the queue into the client.
    let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "priority": "batch", "source_uri": "originals/t1/a" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    assert_eq!(dispatch.count(), 1, "the refused request must not have been dispatched");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM ops.jobs").fetch_one(&pool).await.expect("count jobs");
    assert_eq!(jobs, 1, "a refused request must leave no job row behind");

    // Under the limit, interactive is accepted — otherwise this test would pass with
    // back-pressure wired to always refuse.
    let (s, _) = state(pool.clone(), 1).await;
    let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "priority": "interactive", "source_uri": "originals/t1/a" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_source_uri_outside_the_tenants_own_prefix_is_refused(pool: PgPool) {
    let (s, dispatch) = state(pool.clone(), 0).await;

    // The object exists and belongs to another tenant. This is the one check standing
    // between two tenants' documents while there is no auth, so it is asserted against
    // a key that is really there rather than a missing one — a 404-by-accident would
    // hide a missing check.
    for uri in ["originals/t2/secret", "originals/t1/../t2/secret", "s3://visual-suite/originals/t2/secret"] {
        let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": uri }))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} was accepted: {body}");
    }
    // Inside the prefix but not present: a different answer, so the two cases stay
    // distinguishable to a caller.
    let (status, _) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": "originals/t1/missing" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Both forms of the tenant's own key work, bare and as a bucket URI.
    for uri in ["inbox/t1/c", "s3://visual-suite/originals/t1/a"] {
        let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": uri }))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{uri} was refused: {body}");
    }
    assert_eq!(dispatch.count(), 2);
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_batch_is_validated_whole_and_then_enqueued_whole(pool: PgPool) {
    let (s, dispatch) = state(pool.clone(), 0).await;

    // One bad document rejects the batch and writes nothing: a partially accepted batch
    // is the worst outcome for a caller, who then has to work out which half ran.
    let mixed = json!({
        "options": { "language": "en" },
        "documents": [{ "source_uri": "originals/t1/a" }, { "source_uri": "originals/t2/secret" }],
    });
    let (status, body) = send(&s, post("/v1/batches", mixed)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["text"].as_str().unwrap_or_default().contains("documents[1]"), "the response must say which one: {body}");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM ops.jobs").fetch_one(&pool).await.expect("count jobs");
    assert_eq!(jobs, 0, "a refused batch must create no jobs");
    assert!(dispatch.count() == 0);

    let good = json!({
        "options": { "language": "en", "priority": "batch" },
        "documents": [
            { "source_uri": "originals/t1/a", "external_ref": "one" },
            { "source_uri": "inbox/t1/c", "external_ref": "two", "language": "de" },
        ],
    });
    let (status, body) = send(&s, post("/v1/batches", good)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["job_ids"].as_array().map(Vec::len), Some(2), "{body}");
    assert_eq!(body["failed_job_ids"].as_array().map(Vec::len), Some(0), "{body}");
    assert_eq!(dispatch.count(), 2);

    // Every job carries the batch, so `GET /v1/batches/{id}` can report on it, and the
    // per-document language overrides the batch's.
    let batch_id = body["batch_id"].as_str().expect("batch_id");
    let in_batch: i64 = sqlx::query_scalar("SELECT count(*) FROM ops.jobs WHERE batch_id = $1::uuid")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .expect("count batch jobs");
    assert_eq!(in_batch, 2);
    let langs: Vec<String> = sqlx::query_scalar("SELECT spec->>'language' FROM ops.jobs ORDER BY spec->>'external_ref'")
        .fetch_all(&pool)
        .await
        .expect("languages");
    assert_eq!(langs, vec!["en", "de"]);

    let (status, body) = send(&s, post("/v1/batches", json!({ "options": { "language": "en" }, "documents": [] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn activating_a_dag_list_leaves_exactly_one_active_in_the_scope(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let tenant = TenantId(TENANT.into());
    let list = |version: &str| -> vs_router::DagList {
        serde_json::from_value(json!({
            "version": version,
            "dags": [{ "id": "d", "version": "1", "output": "p",
                       "nodes": [{ "id": "p", "step": "page.parse_e2e", "inputs": ["$page_image"], "timeout_ms": null }] }],
            "routing": { "default_dag": "d", "low_confidence_dag": "d", "min_classifier_confidence": 0.6, "rules": [] },
        }))
        .expect("dag list")
    };
    for v in ["v1", "v2"] {
        assert!(ops.dag_list_insert_candidate(v, Some(&tenant), &list(v), None).await.expect("insert"));
    }
    assert!(ops.active_dag_list(&tenant).await.expect("active").is_none(), "a candidate is not active");

    let admin = |path: &str| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {}", token(&admin_claims())))
            .body(Body::empty())
            .expect("request")
    };
    let (status, body) = send(&s, admin("/v1/dag-lists/v1/activate")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ops.active_dag_list_version(&tenant).await.expect("version").as_deref(), Some("v1"));

    // The second activation must *replace* the first, not join it: a worker reads "the"
    // active list for its tenant, so two active rows would make which DAG runs depend on
    // row order.
    let (status, body) = send(&s, admin("/v1/dag-lists/v2/activate")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ops.active_dag_list_version(&tenant).await.expect("version").as_deref(), Some("v2"));
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM registry.dag_lists WHERE status = 'active' AND tenant_id = $1")
        .bind(TENANT)
        .fetch_one(&pool)
        .await
        .expect("count active");
    assert_eq!(active, 1);

    // Unknown version, and a tenant list that is not in the global scope.
    assert_eq!(send(&s, admin("/v1/dag-lists/nope/activate")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&s, admin("/v1/dag-lists/v1/activate?global=true")).await.0, StatusCode::NOT_FOUND);

    // A token without the admin role does not reach an administrative route, and the
    // activation above stands: a 403 must not have changed anything.
    let not_admin = Request::builder()
        .method("POST")
        .uri("/v1/dag-lists/v1/activate")
        .header("authorization", format!("Bearer {}", token(&claims())))
        .body(Body::empty())
        .expect("request");
    assert_eq!(send(&s, not_admin).await.0, StatusCode::FORBIDDEN);
    assert_eq!(ops.active_dag_list_version(&tenant).await.expect("version").as_deref(), Some("v2"));
}
