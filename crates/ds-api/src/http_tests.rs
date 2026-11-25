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
//! Postgres: `cargo test -p ds-api -- --ignored`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use ds_core::{JobSpec, RunId, TenantId};
use ds_infra::{BlobStore, ObjectBlobStore};
use ds_vault::ops::Ops;

use super::auth_tests::{admin_claims, claims, token, verifier, ORG, SUBJECT};
use super::{router, ApiConfig, AppState, Dispatch};

/// The organization in the test token, which is therefore the tenant.
const TENANT: &str = ORG;
const BUCKET: &str = "document-suite";

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
        // Off: these tests make many requests as one organization, and the limiter has
        // its own tests in `limit`.
        limiter: Arc::new(crate::limit::RateLimiter::new(0.0, 0.0)),
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

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
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

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
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

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_source_uri_outside_the_tenants_own_prefix_is_refused(pool: PgPool) {
    let (s, dispatch) = state(pool.clone(), 0).await;

    // The object exists and belongs to another tenant. This is the one check standing
    // between two tenants' documents while there is no auth, so it is asserted against
    // a key that is really there rather than a missing one — a 404-by-accident would
    // hide a missing check.
    for uri in ["originals/t2/secret", "originals/t1/../t2/secret", "s3://document-suite/originals/t2/secret"] {
        let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": uri }))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} was accepted: {body}");
    }
    // Inside the prefix but not present: a different answer, so the two cases stay
    // distinguishable to a caller.
    let (status, _) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": "originals/t1/missing" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Both forms of the tenant's own key work, bare and as a bucket URI.
    for uri in ["inbox/t1/c", "s3://document-suite/originals/t1/a"] {
        let (status, body) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": uri }))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{uri} was refused: {body}");
    }
    assert_eq!(dispatch.count(), 2);
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
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

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn activating_a_dag_list_leaves_exactly_one_active_in_the_scope(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let tenant = TenantId(TENANT.into());
    let list = |version: &str| -> ds_router::DagList {
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

// ───────────── read scoping inside the organization ─────────────

// The organization boundary held all along; this is the part that did not exist. Every
// test below has two members of **one** organization, which is the case the old API
// could not express: `t1` reading `t1`'s documents was always allowed, whoever asked.

use super::auth_tests::{other_admin_claims, other_member_claims, OTHER_SUBJECT};
use ds_core::HashKey;
use ds_vault::access::{GranteeKind, Visibility};

/// A request as whichever member the claims name.
fn as_member(method: &str, path: &str, claims: &Value, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {}", token(claims)));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).expect("request")
}

/// A document owned by the token's own subject, at the given visibility.
async fn owned_document(ops: &Ops, visibility: Visibility) -> HashKey {
    let hk = HashKey::document(&TenantId(TENANT.into()), &"a".repeat(64));
    let owner = ds_core::Owner { subject: SUBJECT.into(), department: Some("dept-a".into()), workspaces: vec!["ws-red".into()] };
    assert!(ops
        .record_document_owner(&TenantId(TENANT.into()), &hk, Some(&owner), visibility)
        .await
        .expect("record owner"));
    hk
}

/// The read reached the store: there is no vault row behind these hash keys, so a
/// caller who passes the access check gets "not found" and one who does not gets 403.
/// Distinguishing the two is the whole point — a 404 for both would hide a working check
/// from a broken one.
const PASSED_THE_CHECK: StatusCode = StatusCode::NOT_FOUND;

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_private_document_is_not_readable_by_another_member_of_the_same_organization(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let hk = owned_document(&ops, Visibility::Private).await;
    let doc = format!("/v1/documents/{hk}");

    let (status, body) = send(&s, as_member("GET", &doc, &claims(), None)).await;
    assert_eq!(status, PASSED_THE_CHECK, "the owner must reach the store: {body}");

    let (status, body) = send(&s, as_member("GET", &doc, &other_member_claims(), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a colleague read a private document: {body}");
    // The page bytes are the actual payload, so the same check has to be on that route.
    let (status, _) = send(&s, as_member("GET", &format!("{doc}/pages/1"), &other_member_claims(), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the page route is not scoped");

    // And an administrator of the organization does reach it, because `admin_reads_all`
    // defaults to true — the setting that decides it is the deployment's.
    let (status, _) = send(&s, as_member("GET", &doc, &other_admin_claims(), None)).await;
    assert_eq!(status, PASSED_THE_CHECK, "an administrator is refused by default");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn visibility_and_grants_open_a_document_and_close_it_again(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let hk = owned_document(&ops, Visibility::Private).await;
    let (doc, other) = (format!("/v1/documents/{hk}"), other_member_claims());

    // Widened to the whole organization: what every document used to be.
    let (status, body) = send(&s, as_member("PUT", &format!("{doc}/access"), &claims(), Some(json!({ "visibility": "organization" })))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(send(&s, as_member("GET", &doc, &other, None)).await.0, PASSED_THE_CHECK);

    // Narrowed again. A share that cannot be withdrawn is not a share, and this is why
    // the rows are in `ops` and not in the insert-only vault.
    let (status, _) = send(&s, as_member("PUT", &format!("{doc}/access"), &claims(), Some(json!({ "visibility": "private" })))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(send(&s, as_member("GET", &doc, &other, None)).await.0, StatusCode::FORBIDDEN);

    // One named exception, by person.
    let grant = json!({ "kind": "subject", "id": OTHER_SUBJECT });
    let (status, body) = send(&s, as_member("POST", &format!("{doc}/grants"), &claims(), Some(grant.clone()))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(send(&s, as_member("GET", &doc, &other, None)).await.0, PASSED_THE_CHECK, "a grant did not open the document");
    // Granting twice is one grant, not two rows and not an error.
    assert_eq!(send(&s, as_member("POST", &format!("{doc}/grants"), &claims(), Some(grant))).await.0, StatusCode::CREATED);
    assert_eq!(ops.document_grants(&TenantId(TENANT.into()), &hk).await.expect("grants").len(), 1);

    let (status, _) = send(&s, as_member("DELETE", &format!("{doc}/grants/subject/{OTHER_SUBJECT}"), &claims(), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(send(&s, as_member("GET", &doc, &other, None)).await.0, StatusCode::FORBIDDEN, "the grant was withdrawn");

    // A workspace grant reaches a member through a workspace rather than by name.
    let (status, _) =
        send(&s, as_member("POST", &format!("{doc}/grants"), &claims(), Some(json!({ "kind": "workspace", "id": "ws-green" })))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(send(&s, as_member("GET", &doc, &other, None)).await.0, PASSED_THE_CHECK);
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn only_the_owner_or_an_administrator_decides_who_reads_a_document(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let hk = owned_document(&ops, Visibility::Organization).await;
    let access = format!("/v1/documents/{hk}/access");

    // Readable by the whole organization, and still not re-shareable by anyone in it:
    // reading a document and changing who else can are different powers.
    let (status, body) = send(&s, as_member("PUT", &access, &other_member_claims(), Some(json!({ "visibility": "private" })))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        ops.document_access(&TenantId(TENANT.into()), &hk).await.expect("access").expect("row").visibility,
        Visibility::Organization,
        "a refused request changed the visibility anyway"
    );

    let (status, _) = send(&s, as_member("PUT", &access, &other_admin_claims(), Some(json!({ "visibility": "private" })))).await;
    assert_eq!(status, StatusCode::OK, "an administrator must be able to fix a mis-shared document");

    // A visibility that is not one of the four.
    let (status, _) = send(&s, as_member("PUT", &access, &claims(), Some(json!({ "visibility": "everyone" })))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The owner sees who owns it; another member is not handed a personal identifier.
    let (_, body) = send(&s, as_member("GET", &access, &claims(), None)).await;
    assert_eq!(body["owner"].as_str(), Some(SUBJECT));
    let (_, body) = send(&s, as_member("GET", &access, &other_admin_claims(), None)).await;
    assert_eq!(body["owner"].as_str(), Some(SUBJECT), "an administrator may ask whose it is");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_document_listing_answers_what_this_caller_can_read(pool: PgPool) {
    // Once a member cannot necessarily read everything, "what can I read" stops being
    // answerable by the caller and has to be answered here.
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let tenant = TenantId(TENANT.into());
    let owner = ds_core::Owner { subject: SUBJECT.into(), department: Some("dept-a".into()), workspaces: vec!["ws-red".into()] };
    let mut keys = Vec::new();
    for (i, v) in [Visibility::Private, Visibility::Organization, Visibility::Workspace, Visibility::Department].into_iter().enumerate() {
        let hk = HashKey::document(&tenant, &format!("{i}").repeat(64));
        ops.record_document_owner(&tenant, &hk, Some(&owner), v).await.expect("record");
        keys.push((hk, v));
    }
    // A fifth with no owner at all: the shape every document had before scoping existed.
    let legacy = HashKey::document(&tenant, &"f".repeat(64));
    ops.record_document_owner(&tenant, &legacy, None, Visibility::Private).await.expect("record");

    let hks = |body: &Value| -> Vec<String> {
        body["documents"].as_array().expect("documents").iter().map(|d| d["document_hk"].as_str().expect("hk").to_string()).collect()
    };
    let (status, body) = send(&s, as_member("GET", "/v1/documents", &claims(), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hks(&body).len(), 5, "the owner reads all of their own, and the ownerless one: {body}");

    let (_, body) = send(&s, as_member("GET", "/v1/documents", &other_member_claims(), None)).await;
    let visible = hks(&body);
    // Organization-wide, plus the ownerless one. Not the private one, not the owner's
    // department (`dept-a` against `dept-b`), not the owner's workspace (`ws-red`
    // against `ws-green`).
    assert_eq!(visible.len(), 2, "another member saw {visible:?}");
    assert!(visible.contains(&legacy.to_string()));
    assert!(visible.contains(&keys[1].0.to_string()));

    // A grant shows up in the listing too, which is the case the SQL predicate and
    // `may_read` have to agree about.
    ops.grant_document(&tenant, &keys[0].0, GranteeKind::Subject, OTHER_SUBJECT, Some(SUBJECT)).await.expect("grant");
    let (_, body) = send(&s, as_member("GET", "/v1/documents", &other_member_claims(), None)).await;
    assert_eq!(hks(&body).len(), 3, "a granted document is missing from the listing: {body}");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_jobs_status_belongs_to_the_person_who_asked_for_it(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let (status, created) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": "originals/t1/a" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    let job = format!("/v1/jobs/{}", created["job_id"].as_str().expect("job id"));

    assert_eq!(send(&s, as_member("GET", &job, &claims(), None)).await.0, StatusCode::OK, "the asker");
    let (status, body) = send(&s, as_member("GET", &job, &other_member_claims(), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "another member read someone else's job: {body}");
    assert_eq!(send(&s, as_member("GET", &job, &other_admin_claims(), None)).await.0, StatusCode::OK, "an administrator");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn erasure_removes_the_person_and_leaves_the_organizations_work(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let ops = Ops { pool: pool.clone() };
    let tenant = TenantId(TENANT.into());
    let (status, created) = send(&s, post("/v1/documents", json!({ "language": "en", "source_uri": "originals/t1/a" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    let hk = owned_document(&ops, Visibility::Private).await;

    // Another member cannot erase a person; the person themselves can, without an
    // administrator relaying it.
    let path = format!("/v1/owners/{SUBJECT}");
    assert_eq!(send(&s, as_member("DELETE", &path, &other_member_claims(), None)).await.0, StatusCode::FORBIDDEN);
    let (status, body) = send(&s, as_member("DELETE", &path, &claims(), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["jobs"], 1);
    assert_eq!(body["documents"], 1);

    // The job row stays — the organization's record of what it processed is not the
    // person's to erase — and carries no subject any more, in the column or in the spec.
    let (subject, in_spec): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT owner_subject, spec->'owner'->>'subject' FROM ops.jobs").fetch_one(&pool).await.expect("job");
    assert_eq!(subject, None, "the subject survived erasure in its own column");
    assert_eq!(in_spec, None, "the subject survived erasure inside the spec");

    // The document is still there and is now the organization's: a private document whose
    // owner no longer exists would otherwise be readable by nobody.
    let access = ops.document_access(&tenant, &hk).await.expect("access").expect("row");
    assert_eq!(access.owner_subject, None);
    assert_eq!(access.visibility, Visibility::Organization);
    assert_eq!(send(&s, as_member("GET", &format!("/v1/documents/{hk}"), &other_member_claims(), None)).await.0, PASSED_THE_CHECK);

    // And nothing in the vault had to be rewritten, which is what makes this short: no
    // satellite stores an owner.
    let vault_owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'vault' AND column_name IN ('owner_subject', 'owner')",
    )
    .fetch_one(&pool)
    .await
    .expect("columns");
    assert_eq!(vault_owners, 0, "an owner column appeared in the insert-only vault, where erasure cannot reach it");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_revocation_can_be_recorded_by_its_owner_and_an_administrator(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    s.auth.with_revocations(s.ops.clone());

    // Anyone may end their own session: "log me out everywhere".
    let own = json!({ "kind": "subject", "id": SUBJECT });
    let (status, body) = send(&s, as_member("POST", "/v1/auth/revocations", &claims(), Some(own))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Not someone else's, without the admin role.
    let theirs = json!({ "kind": "subject", "id": OTHER_SUBJECT });
    assert_eq!(send(&s, as_member("POST", "/v1/auth/revocations", &claims(), Some(theirs.clone()))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(send(&s, as_member("POST", "/v1/auth/revocations", &other_admin_claims(), Some(theirs))).await.0, StatusCode::CREATED);

    // It took effect on this copy immediately, rather than at the next refresh.
    assert_eq!(s.auth.revocation_count().await, 2);
    let (status, body) = send(&s, as_member("GET", "/v1/auth/revocations", &admin_claims(), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revocations"], 2, "{body}");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_batch_and_its_jobs_belong_to_whoever_asked_for_them(pool: PgPool) {
    let (s, _) = state(pool.clone(), 0).await;
    let batch = json!({ "options": { "language": "en" }, "documents": [{ "source_uri": "originals/t1/a" }, { "source_uri": "originals/t1/b" }] });
    let (status, created) = send(&s, post("/v1/batches", batch)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    let path = format!("/v1/batches/{}", created["batch_id"].as_str().expect("batch id"));

    let (status, body) = send(&s, as_member("GET", &path, &claims(), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["jobs"].as_array().expect("jobs").len(), 2, "the asker sees their own batch whole");

    // Another member of the same organization: the batch is not theirs, and its job ids,
    // states and external refs are not theirs to read either.
    let (status, body) = send(&s, as_member("GET", &path, &other_member_claims(), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(send(&s, as_member("GET", &path, &other_admin_claims(), None)).await.0, StatusCode::OK, "an administrator");
}

#[sqlx::test(migrator = "ds_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn skip_if_processed_does_not_hand_over_another_members_run(pool: PgPool) {
    // A document hash key is derived from the content, so two members uploading the same
    // file reach the same key. Handing the second one the first one's job would reveal
    // that a colleague uploaded that file *and* let them watch a run that is not theirs.
    let (s, dispatch) = state(pool.clone(), 0).await;
    let body = json!({ "language": "en", "source_uri": "originals/t1/a" });
    let upload = |claims: &Value| as_member("POST", "/v1/documents?skip_if_processed=true", claims, Some(body.clone()));

    let (status, first) = send(&s, upload(&claims())).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    // The same caller again: their own run, replayed rather than repeated.
    let (status, again) = send(&s, upload(&claims())).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["job_id"], first["job_id"]);

    // Another member: a new run of the same document, not a window onto the first.
    let (status, theirs) = send(&s, upload(&other_member_claims())).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{theirs}");
    assert_ne!(theirs["job_id"], first["job_id"], "the second member was handed the first member's job");
    assert_eq!(dispatch.count(), 2, "the second upload must actually run");

    // Both jobs are the same document — reprocessing, which is a supported shape — and
    // each caller sees only their own.
    let hks: Vec<Option<String>> = sqlx::query_scalar("SELECT document_hk FROM ops.jobs ORDER BY created_at")
        .fetch_all(&pool)
        .await
        .expect("jobs");
    assert_eq!(hks.len(), 2);
    assert_eq!(hks[0], hks[1], "the same file is the same document, whoever uploaded it");
}
