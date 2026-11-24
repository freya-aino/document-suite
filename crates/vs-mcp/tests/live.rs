//! The tools against the real API: the real `vs_api::router`, real Postgres, real signed
//! tokens, on a real socket.
//!
//! `tests/tools.rs` asserts what this server *sends*; this file asserts that what it sends
//! is what the suite actually answers. It is the test that catches the failure a fake
//! cannot: a route renamed, a field spelled differently, a status code changed, an answer
//! that is a 204 rather than a body. Without it this crate's idea of the API would drift
//! from the API.
//!
//! Two people appear, in the same organization, because that is the property worth proving
//! end to end: Bob holds a valid token and still cannot read Alice's document, and the
//! tool says so in words a model can act on. The token is the only thing that decides —
//! there is no tenant argument to get wrong.
//!
//! Needs Postgres: `cargo test -p vs-mcp -- --ignored`.

use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use sqlx::PgPool;

use vs_api::auth::Auth;
use vs_api::{router, AppState, Dispatch};
use vs_core::{JobSpec, Owner, RunId, TenantId};
use vs_infra::config::{ApiConfig, AuthConfig};
use vs_infra::{BlobStore, ObjectBlobStore};
use vs_mcp::server::Server;
use vs_mcp::suite::{Suite, TokenSource};
use vs_mcp::tools::Limits;
use vs_vault::ops::Ops;

const AUDIENCE: &str = "vs-api";
const CLIENT_ID: &str = "vs-api";
const ORG: &str = "t1";
const ALICE: &str = "person-1";
const BOB: &str = "person-2";
const BUCKET: &str = "visual-suite";

/// The same fixture keypair `vs-api`'s own tests use, read from that crate rather than
/// copied: two JWKS that drift apart would make this test pass against a verifier no
/// deployment runs.
fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../vs-api/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// A realm, served the way Keycloak serves one: a discovery document that names its own
/// issuer and a JWKS endpoint holding the fixture's public key.
///
/// This rather than a verifier built over a fixed key set, for two reasons. `Auth`'s
/// fixed-key constructor is test-only *inside* `vs-api`, and reaching it from here would
/// mean a feature flag on a production crate that exists for one test; and discovery is
/// part of what a deployment does, so a test that skips it is a test of less. The issuer
/// is this socket, so the token's `iss` and the configured issuer agree the way they must.
async fn serve_realm() -> String {
    let jwks: Value = serde_json::from_slice(&fixture("test-jwks.json")).expect("test JWKS");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let issuer = format!("http://{addr}/realms/koki");

    let doc = json!({
        "issuer": issuer,
        "jwks_uri": format!("{issuer}/protocol/openid-connect/certs"),
        "authorization_endpoint": format!("{issuer}/protocol/openid-connect/auth"),
        "token_endpoint": format!("{issuer}/protocol/openid-connect/token"),
    });
    let app = axum::Router::new()
        .route("/realms/koki/.well-known/openid-configuration", axum::routing::get(move || async move { axum::Json(doc) }))
        .route("/realms/koki/protocol/openid-connect/certs", axum::routing::get(move || async move { axum::Json(jwks) }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    issuer
}

fn auth_config(issuer: &str) -> AuthConfig {
    AuthConfig {
        issuer: issuer.into(),
        audience: vec![AUDIENCE.into()],
        client_id: Some(CLIENT_ID.into()),
        org_claim: "organization".into(),
        department_claim: "department".into(),
        workspaces_claim: "workspaces".into(),
        admin_role: "vs-admin".into(),
        jwks_refresh_s: 300,
        leeway_s: 30,
    }
}

async fn verifier(issuer: &str) -> Arc<Auth> {
    Auth::discover(auth_config(issuer)).await.expect("the fixture realm should be discoverable")
}

/// A real token for one member of organization `t1`.
fn token(issuer: &str, subject: &str, department: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "sub": subject,
        "iss": issuer,
        "aud": AUDIENCE,
        "azp": CLIENT_ID,
        "typ": "Bearer",
        "jti": uuid::Uuid::now_v7(),
        "sid": uuid::Uuid::now_v7(),
        "iat": now,
        "exp": now + 300,
        "organization": ORG,
        "department": department,
        "workspaces": ["ws-red"],
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key-1".into());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_rsa_pem(&fixture("test-key-1.pem")).expect("fixture key")).expect("sign")
}

/// Accepts a job and does nothing with it.
///
/// The pipeline is not under test here — the API's answers are — and running it would mean
/// models and a GPU. What the tools see is a job that stays `received`, which is a state
/// they have to handle anyway.
struct NoDispatch;

#[async_trait]
impl Dispatch for NoDispatch {
    async fn dispatch(&self, _job_id: uuid::Uuid, _run_id: RunId, _spec: JobSpec) -> anyhow::Result<()> {
        Ok(())
    }

    fn in_flight(&self) -> u64 {
        0
    }
}

/// Serves the real router on a real socket and returns its base URL.
async fn serve_api(pool: PgPool, issuer: &str) -> String {
    let blob = Arc::new(ObjectBlobStore::new(Arc::new(object_store::memory::InMemory::new())));
    for key in ["originals/t1/a.pdf", "originals/t1/b.pdf"] {
        blob.put(key, bytes::Bytes::from_static(b"%PDF-1.7 fake"), "application/pdf").await.expect("seed blob");
    }
    let state = AppState {
        ops: Arc::new(Ops { pool }),
        auth: verifier(issuer).await,
        dispatch: Arc::new(NoDispatch),
        blob,
        bucket: BUCKET.into(),
        api: ApiConfig { interactive_max_pending: 10, ..Default::default() },
        // Off: this test makes many requests as one organization, and the limiter has its
        // own tests.
        limiter: Arc::new(vs_api::limit::RateLimiter::new(0.0, 0.0)),
        steps: None,
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder().handle(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    format!("http://{addr}")
}

/// Does the one thing ingest does that the access tools depend on: record who uploaded
/// the document.
///
/// Running the pipeline here would mean models and a GPU, and this file is about the API's
/// answers. But the access row is **not** written when a job is accepted — it is written
/// when ingest first knows the document's hash key — and the first live run of these tests
/// proved it, by refusing every share with "no access record for this document". So the
/// test writes that row through the vault's own function, which is the one ingest calls.
async fn record_owner(pool: &PgPool, hk: &str, subject: &str, department: &str) {
    let ops = Ops { pool: pool.clone() };
    let owner = Owner { subject: subject.into(), department: Some(department.into()), workspaces: vec!["ws-red".into()] };
    ops.record_document_owner(
        &TenantId(ORG.into()),
        &vs_vault::hash_key_from_hex(hk).expect("a hash key"),
        Some(&owner),
        vs_vault::access::Visibility::Private,
    )
    .await
    .expect("record the owner");
}

/// Whether a document appears in a caller's own listing, which is the read probe these
/// tests use: a document with no completed run has nothing to `get_document` for, but who
/// may see it is decided by exactly the rules under test.
async fn lists(server: &Server, token: &str, hk: &str) -> bool {
    let listed = call(server, "list_documents", json!({ "page_size": 500 }), token).await;
    assert_eq!(listed["isError"], false, "{}", text_of(&listed));
    listed["structuredContent"]["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .any(|d| d["document_hk"].as_str() == Some(hk))
}

fn mcp(base: &str) -> Server {
    Server {
        suite: Suite::new(base, TokenSource::PerRequest, std::time::Duration::from_secs(10)).expect("client"),
        limits: Limits { poll_interval: std::time::Duration::from_millis(50), ..Limits::default() },
    }
}

async fn call(server: &Server, name: &str, args: Value, token: &str) -> Value {
    let msg = serde_json::from_value(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    }))
    .expect("a request");
    let out = server.handle(msg, Some(token)).await.expect("a reply");
    assert!(out.error.is_none(), "{name} should not be a protocol error: {:?}", out.error);
    out.result.expect("a result")
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().expect("text").to_string()
}

/// Upload, follow, list, and read the access record — every tool that the API answers
/// without a pipeline having run.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn the_tools_speak_the_api_the_suite_actually_serves(pool: PgPool) {
    let issuer = serve_realm().await;
    let base = serve_api(pool.clone(), &issuer).await;
    let s = mcp(&base);
    let alice = token(&issuer, ALICE, "dept-a");

    // Submitting a document: the route, the body and the answer's field names.
    let accepted = call(&s, "upload_document", json!({ "source_uri": "originals/t1/a.pdf", "language": "en", "skip_if_processed": true }), &alice).await;
    assert_eq!(accepted["isError"], false, "{}", text_of(&accepted));
    let job_id = accepted["structuredContent"]["job_id"].as_str().expect("a job id").to_string();
    let document_hk = accepted["structuredContent"]["document_hk"].as_str().expect("a document hash key").to_string();
    assert_eq!(document_hk.len(), 64, "the API's hash key must be the shape the tools validate");

    // The job is readable and its state is one the tool knows how to describe.
    let job = call(&s, "get_job", json!({ "job_id": job_id }), &alice).await;
    assert_eq!(job["isError"], false, "{}", text_of(&job));
    let state = job["structuredContent"]["state"].as_str().expect("a state");
    assert!(
        ["received", "ingesting", "pages_processing", "stitching", "doc_processing", "writing", "done", "partial", "failed"].contains(&state),
        "the API answered a state the tools do not describe: {state}"
    );

    // Once ingest has recorded the uploader, the listing contains the document, with the
    // field names the tool renders.
    record_owner(&pool, &document_hk, ALICE, "dept-a").await;
    let listed = call(&s, "list_documents", json!({}), &alice).await;
    assert_eq!(listed["isError"], false, "{}", text_of(&listed));
    let docs = listed["structuredContent"]["documents"].as_array().expect("documents");
    assert!(
        docs.iter().any(|d| d["document_hk"].as_str() == Some(document_hk.as_str())),
        "the uploaded document should be readable by its uploader: {listed}"
    );
    assert!(text_of(&listed).contains(&document_hk), "the rendered text should name it too");

    // The access record, through the tool that reads it.
    let access = call(&s, "get_document_access", json!({ "document_hk": document_hk }), &alice).await;
    assert_eq!(access["isError"], false, "{}", text_of(&access));
    assert!(access["structuredContent"]["visibility"].is_string(), "{access}");
}

/// The property per-person access exists for, proved through the tools: a valid token is
/// not the same as permission, and the refusal tells the model what to do.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn a_second_member_cannot_read_the_first_members_document_until_it_is_shared(pool: PgPool) {
    let issuer = serve_realm().await;
    let base = serve_api(pool.clone(), &issuer).await;
    let s = mcp(&base);
    let alice = token(&issuer, ALICE, "dept-a");
    // Same organization, different person, different department.
    let bob = token(&issuer, BOB, "dept-b");

    let accepted = call(&s, "upload_document", json!({ "source_uri": "originals/t1/a.pdf", "language": "en", "skip_if_processed": true }), &alice).await;
    let hk = accepted["structuredContent"]["document_hk"].as_str().expect("a hash key").to_string();
    record_owner(&pool, &hk, ALICE, "dept-a").await;

    assert!(lists(&s, &alice, &hk).await, "the uploader must see their own document");
    assert!(!lists(&s, &bob, &hk).await, "a private document must not be listed to another member of the organization");

    // Bob holds a valid token for the same organization and is still refused, with advice
    // that names the remedy rather than suggesting a retry.
    let refused = call(&s, "get_document_access", json!({ "document_hk": hk }), &bob).await;
    assert_eq!(refused["isError"], true, "{refused}");
    let text = text_of(&refused);
    assert!(text.contains("share_document") || text.contains("grant_access"), "the refusal must name the remedy: {text}");

    // Alice grants it. Bob can then see it, and nothing about his token changed.
    let granted = call(&s, "grant_access", json!({ "document_hk": hk, "kind": "subject", "id": BOB }), &alice).await;
    assert_eq!(granted["isError"], false, "{}", text_of(&granted));
    assert!(lists(&s, &bob, &hk).await, "a granted document should be readable");

    // Revoking takes it away again, which is what makes the tool safe to advertise.
    let revoked = call(&s, "revoke_access", json!({ "document_hk": hk, "kind": "subject", "id": BOB }), &alice).await;
    assert_eq!(revoked["isError"], false, "{}", text_of(&revoked));
    assert!(!lists(&s, &bob, &hk).await, "a withdrawn grant must take the access with it");
}

/// Bob cannot share what is not his, and the API is what decides that — not this server.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn only_the_owner_can_widen_a_document(pool: PgPool) {
    let issuer = serve_realm().await;
    let base = serve_api(pool.clone(), &issuer).await;
    let s = mcp(&base);
    let alice = token(&issuer, ALICE, "dept-a");
    let bob = token(&issuer, BOB, "dept-b");

    let accepted = call(&s, "upload_document", json!({ "source_uri": "originals/t1/a.pdf", "language": "en", "skip_if_processed": true }), &alice).await;
    let hk = accepted["structuredContent"]["document_hk"].as_str().expect("a hash key").to_string();
    record_owner(&pool, &hk, ALICE, "dept-a").await;

    let refused = call(&s, "share_document", json!({ "document_hk": hk, "visibility": "organization" }), &bob).await;
    assert_eq!(refused["isError"], true, "a non-owner must not be able to share: {refused}");

    let allowed = call(&s, "share_document", json!({ "document_hk": hk, "visibility": "organization" }), &alice).await;
    assert_eq!(allowed["isError"], false, "{}", text_of(&allowed));

    // Now Bob sees it, through visibility rather than a grant.
    assert!(lists(&s, &bob, &hk).await, "an organization-visible document should be readable by every member");
}

/// An expired token is refused by the suite, and the tool's advice says the one thing that
/// is true: this server cannot fix it.
#[sqlx::test(migrator = "vs_vault::MIGRATOR")]
#[ignore = "needs Postgres from `devenv up`"]
async fn an_expired_token_is_refused_with_advice_that_does_not_promise_a_refresh(pool: PgPool) {
    let issuer = serve_realm().await;
    let base = serve_api(pool, &issuer).await;
    let s = mcp(&base);

    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "sub": ALICE, "iss": issuer, "aud": AUDIENCE, "azp": CLIENT_ID, "typ": "Bearer",
        "jti": uuid::Uuid::now_v7(), "sid": uuid::Uuid::now_v7(),
        // Well past the verifier's leeway.
        "iat": now - 7200, "exp": now - 3600,
        "organization": ORG, "department": "dept-a", "workspaces": ["ws-red"],
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key-1".into());
    let expired = jsonwebtoken::encode(&header, &claims, &EncodingKey::from_rsa_pem(&fixture("test-key-1.pem")).expect("key")).expect("sign");

    let result = call(&s, "list_documents", json!({}), &expired).await;
    assert_eq!(result["isError"], true);
    let text = text_of(&result);
    assert!(text.contains("refused the token") || text.contains("expired"), "{text}");
    assert!(text.contains("cannot refresh it"), "the advice must not imply this server can get a new token: {text}");
}
