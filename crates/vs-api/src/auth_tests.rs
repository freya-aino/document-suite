//! What authentication refuses.
//!
//! These are the attacks and the mistakes, not the happy path: a token signed with the
//! public key as an HMAC secret, a token from the realm next door, an expired one, one
//! for a different audience, a refresh token used as an access token, and an
//! organization id that would walk out of its own blob prefix. Every token here is
//! really signed and really verified against a real JWKS — the fixtures are an actual
//! RSA keypair (`fixtures/README.md`), so nothing in this file is asserted against a
//! stubbed verifier.
//!
//! Needs no database: all of it either calls the verifier directly or is refused before a
//! handler runs, and the one route that answers 200 is `/metrics`.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};use axum::routing::get;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use vs_infra::config::{ApiConfig, AuthConfig};
use vs_vault::ops::Ops;

use super::auth::{Auth, AuthError, Principal};
use super::{router, AppState};

pub(crate) const ISSUER: &str = "https://sso.test/realms/koki";
pub(crate) const AUDIENCE: &str = "vs-api";
pub(crate) const CLIENT_ID: &str = "vs-api";
pub(crate) const ADMIN_ROLE: &str = "vs-admin";
/// The organization claim's value, and so the tenant. The same `t1` the HTTP tests use.
pub(crate) const ORG: &str = "t1";
pub(crate) const SUBJECT: &str = "person-1";

const KEY_1: &[u8] = include_bytes!("../fixtures/test-key-1.pem");
const KEY_2: &[u8] = include_bytes!("../fixtures/test-key-2.pem");
const JWKS_1: &str = include_str!("../fixtures/test-jwks.json");
const JWKS_2: &str = include_str!("../fixtures/test-jwks-key-2.json");

pub(crate) fn jwks() -> JwkSet {
    serde_json::from_str(JWKS_1).expect("test JWKS")
}

pub(crate) fn auth_config() -> AuthConfig {
    AuthConfig {
        issuer: ISSUER.into(),
        audience: vec![AUDIENCE.into()],
        client_id: Some(CLIENT_ID.into()),
        org_claim: "organization".into(),
        department_claim: "department".into(),
        workspaces_claim: "workspaces".into(),
        admin_role: ADMIN_ROLE.into(),
        jwks_refresh_s: 300,
        leeway_s: 30,
    }
}

pub(crate) fn verifier() -> Arc<Auth> {
    Auth::with_keys(auth_config(), jwks())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A well-formed access token's claims, as Keycloak would mint them.
pub(crate) fn claims() -> Value {
    json!({
        "sub": SUBJECT,
        "iss": ISSUER,
        "aud": AUDIENCE,
        "azp": CLIENT_ID,
        "typ": "Bearer",
        "jti": "11111111-1111-1111-1111-111111111111",
        "iat": now(),
        "exp": now() + 300,
        "organization": ORG,
        "department": "dept-a",
        "workspaces": ["ws-red", "ws-blue"],
        "realm_access": { "roles": ["offline_access"] },
    })
}

/// The same, holding the admin role on the `vs-api` client.
pub(crate) fn admin_claims() -> Value {
    let mut c = claims();
    c["resource_access"] = json!({ CLIENT_ID: { "roles": [ADMIN_ROLE] } });
    c
}

/// Signs claims with a fixture key, under whatever `kid` and algorithm are asked for —
/// including combinations a realm would never issue, which is the point.
pub(crate) fn sign_with(claims: &Value, kid: &str, alg: Algorithm, key: EncodingKey) -> String {
    let mut header = Header::new(alg);
    header.kid = Some(kid.into());
    jsonwebtoken::encode(&header, claims, &key).expect("sign")
}

pub(crate) fn token(claims: &Value) -> String {
    sign_with(claims, "test-key-1", Algorithm::RS256, EncodingKey::from_rsa_pem(KEY_1).expect("fixture key 1"))
}

/// `AppState` with a lazy pool: nothing here reaches a handler that queries, and the one
/// that answers 200 (`/metrics`) renders a recorder.
pub(crate) fn state() -> AppState {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool");
    AppState {
        ops: Arc::new(Ops { pool }),
        auth: verifier(),
        bus: Arc::new(super::http_tests::FakeBus::idle()),
        blob: Arc::new(vs_infra::ObjectBlobStore::new(Arc::new(object_store::memory::InMemory::new()))),
        bucket: "visual-suite".into(),
        api: ApiConfig::default(),
        steps: None,
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder().handle(),
    }
}

async fn status_of(state: &AppState, req: Request<Body>) -> StatusCode {
    router(state.clone()).oneshot(req).await.expect("router").status()
}

fn get_with(path: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).expect("request")
}

// ───────────── the routes ─────────────

/// Every route that touches a tenant's work refuses an unauthenticated request.
///
/// The list is the router's, route for route. The failure this catches is a route added
/// outside the authentication layer: that route would answer *something* here instead of
/// 401, and it would be the one route through which anyone could read anyone's
/// documents. The extractors cannot save it — `Tenant` has no header to fall back to —
/// but a route that takes no extractor at all would simply run.
#[tokio::test]
async fn every_route_but_the_health_probes_requires_a_token() {
    let s = state();
    let protected = [
        ("POST", "/v1/documents"),
        ("POST", "/v1/batches"),
        ("GET", "/v1/jobs/00000000-0000-0000-0000-000000000000"),
        ("GET", "/v1/batches/00000000-0000-0000-0000-000000000000"),
        ("GET", "/v1/documents/abc"),
        ("GET", "/v1/documents/abc/pages/1"),
        ("GET", "/v1/dag-lists"),
        ("POST", "/v1/dag-lists"),
        ("POST", "/v1/dag-lists/v1/activate"),
        ("GET", "/metrics"),
    ];
    for (method, path) in protected {
        let req = Request::builder().method(method).uri(path).body(Body::empty()).expect("request");
        let res = router(s.clone()).oneshot(req).await.expect("router");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {path} answered without a token");
        // RFC 6750: a client library needs to be told the scheme to know what to do.
        assert_eq!(
            res.headers().get("www-authenticate").and_then(|v| v.to_str().ok()),
            Some("Bearer"),
            "{method} {path} refused without a WWW-Authenticate challenge"
        );
    }

    // The probes stay open: a load balancer holds no token, and neither answer says
    // anything about a tenant.
    assert_eq!(status_of(&s, get_with("/healthz", None)).await, StatusCode::OK);
}

/// `/metrics` is on the public internet here, so it is an administrative route.
///
/// Request volumes, upload sizes and error rates are a map of the deployment; the series
/// are per route, so an unauthenticated scrape is a free read of how much work every
/// tenant is sending.
#[tokio::test]
async fn metrics_needs_the_admin_role_and_a_plain_token_is_not_enough() {
    let s = state();

    let res = router(s.clone()).oneshot(get_with("/metrics", Some(&token(&claims())))).await.expect("router");
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "a verified token with no admin role read /metrics");
    // 403, not 401: the token is fine, the caller is not entitled. A client that
    // refreshes its token on a 401 would otherwise loop.
    assert!(res.headers().get("www-authenticate").is_none(), "a 403 must not challenge for a new token");

    let res = router(s.clone()).oneshot(get_with("/metrics", Some(&token(&admin_claims())))).await.expect("router");
    assert_eq!(res.status(), StatusCode::OK, "the admin role was refused its own route");

    // The role has to be *this* API's. A realm role of the same name is accepted (the
    // operator's choice of where to put it), another client's is not.
    let mut elsewhere = claims();
    elsewhere["resource_access"] = json!({ "some-other-client": { "roles": [ADMIN_ROLE] } });
    assert_eq!(status_of(&s, get_with("/metrics", Some(&token(&elsewhere)))).await, StatusCode::FORBIDDEN);

    let mut realm_admin = claims();
    realm_admin["realm_access"] = json!({ "roles": [ADMIN_ROLE] });
    assert_eq!(status_of(&s, get_with("/metrics", Some(&token(&realm_admin)))).await, StatusCode::OK);
}

#[tokio::test]
async fn a_bearer_header_that_is_not_one_is_refused() {
    let s = state();
    let t = token(&admin_claims());
    for header in ["", "Bearer", "Bearer ", "Basic dXNlcjpwYXNz", &format!("Token {t}"), &format!("Bearer {t} extra")] {
        let req = Request::builder()
            .method("GET")
            .uri("/metrics")
            .header("authorization", header)
            .body(Body::empty())
            .expect("request");
        assert_eq!(status_of(&s, req).await, StatusCode::UNAUTHORIZED, "{header:?} was accepted as a bearer token");
    }
    // Case in the scheme is not significant (RFC 7235).
    let req = Request::builder().method("GET").uri("/metrics").header("authorization", format!("bEaReR {t}")).body(Body::empty()).expect("request");
    assert_eq!(status_of(&s, req).await, StatusCode::OK);
}

// ───────────── the token ─────────────

#[tokio::test]
async fn a_valid_token_resolves_to_the_organization_the_person_and_their_roles() {
    let p = verifier().verify(&token(&admin_claims())).await.expect("verify");
    assert_eq!(
        p,
        Principal {
            subject: SUBJECT.into(),
            // The tenant is the organization claim, and nothing else: not a header, not
            // a body, not the subject.
            tenant: vs_core::TenantId(ORG.into()),
            department: Some("dept-a".into()),
            workspaces: vec!["ws-red".into(), "ws-blue".into()],
            roles: vec!["offline_access".into(), ADMIN_ROLE.into()],
            admin: true,
            token_id: Some("11111111-1111-1111-1111-111111111111".into()),
        }
    );
    // What gets recorded on a job, so read scoping within the organization can be
    // enforced later without re-plumbing.
    let owner = p.owner();
    assert_eq!(owner.subject, SUBJECT);
    assert_eq!(owner.department.as_deref(), Some("dept-a"));
    assert_eq!(owner.workspaces, vec!["ws-red", "ws-blue"]);
}

/// The forgery that the algorithm allowlist exists for.
///
/// A JWKS is public. If `HS256` were accepted, anyone could take the modulus out of it,
/// use those bytes as an HMAC secret, sign a token naming themselves an admin of any
/// organization, and this API would verify it with the same bytes. Two things refuse it
/// here — the allowlist, and `jsonwebtoken`'s own key-family check — and the assertion
/// is that the request is refused, whichever one fires.
#[tokio::test]
async fn a_token_signed_with_the_public_key_as_an_hmac_secret_is_refused() {
    let jwks = jwks();
    let modulus = match &jwks.keys[0].algorithm {
        jsonwebtoken::jwk::AlgorithmParameters::RSA(rsa) => rsa.n.clone(),
        other => panic!("fixture is not an RSA key: {other:?}"),
    };
    let mut forged = admin_claims();
    forged["organization"] = json!("some-other-org");

    for secret in [modulus.as_bytes().to_vec(), base64_url(&modulus)] {
        let token = sign_with(&forged, "test-key-1", Algorithm::HS256, EncodingKey::from_secret(&secret).expect("hmac key"));
        let err = verifier().verify(&token).await.expect_err("an HMAC token was accepted");
        assert_eq!(err.reason(), "alg", "refused for the wrong reason: {err:?}");
        assert_eq!(status_of(&state(), get_with("/metrics", Some(&token))).await, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn a_token_the_realm_did_not_sign_is_refused() {
    // The right `kid`, the wrong private key: a forged signature.
    let forged = sign_with(&admin_claims(), "test-key-1", Algorithm::RS256, EncodingKey::from_rsa_pem(KEY_2).expect("fixture key 2"));
    let err = verifier().verify(&forged).await.expect_err("a forged signature was accepted");
    assert_eq!(err.reason(), "signature", "{err:?}");

    // A key the realm does not publish at all.
    let unknown = sign_with(&admin_claims(), "test-key-2", Algorithm::RS256, EncodingKey::from_rsa_pem(KEY_2).expect("fixture key 2"));
    assert_eq!(verifier().verify(&unknown).await.expect_err("unknown kid").reason(), "unknown_key");

    // No `kid` at all: there is no way to know which key to check it against, and
    // "try them all" is how a withdrawn key keeps working.
    let mut header = Header::new(Algorithm::RS256);
    header.kid = None;
    let no_kid = jsonwebtoken::encode(&header, &admin_claims(), &EncodingKey::from_rsa_pem(KEY_1).expect("key 1")).expect("sign");
    assert_eq!(verifier().verify(&no_kid).await.expect_err("no kid").reason(), "kid");

    for junk in ["", "not-a-token", "a.b.c", "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ4In0."] {
        assert!(verifier().verify(junk).await.is_err(), "{junk:?} was accepted");
    }
}

#[tokio::test]
async fn a_token_for_another_realm_or_another_audience_is_refused() {
    let mut other_issuer = admin_claims();
    other_issuer["iss"] = json!("https://sso.test/realms/somewhere-else");
    assert_eq!(verifier().verify(&token(&other_issuer)).await.expect_err("issuer").reason(), "issuer");

    // Keycloak hands every client in a realm a token signed by the same key. Without the
    // audience check, a token minted for an unrelated client would open this API — which
    // is why `auth.audience` has no default.
    let mut other_aud = admin_claims();
    other_aud["aud"] = json!("some-other-service");
    assert_eq!(verifier().verify(&token(&other_aud)).await.expect_err("audience").reason(), "audience");

    let mut no_aud = admin_claims();
    no_aud.as_object_mut().expect("object").remove("aud");
    assert_eq!(verifier().verify(&token(&no_aud)).await.expect_err("missing aud").reason(), "missing_claim");

    // An `aud` array containing ours is how Keycloak writes it when more than one
    // audience mapper applies.
    let mut multi = admin_claims();
    multi["aud"] = json!(["account", AUDIENCE]);
    assert!(verifier().verify(&token(&multi)).await.is_ok());
}

#[tokio::test]
async fn a_token_outside_its_own_validity_window_is_refused() {
    let mut expired = admin_claims();
    expired["exp"] = json!(now() - 600);
    assert_eq!(verifier().verify(&token(&expired)).await.expect_err("expired").reason(), "expired");

    // `nbf` is not validated by default in `jsonwebtoken`, so this is the assertion that
    // the verifier turns it on: a token minted for later must not work now.
    let mut not_yet = admin_claims();
    not_yet["nbf"] = json!(now() + 600);
    assert_eq!(verifier().verify(&token(&not_yet)).await.expect_err("nbf").reason(), "not_yet_valid");

    // Inside the configured leeway, a clock a few seconds apart is not a failure.
    let mut just_expired = admin_claims();
    just_expired["exp"] = json!(now() - 5);
    assert!(verifier().verify(&token(&just_expired)).await.is_ok(), "30 s of leeway was not applied");

    let mut no_exp = admin_claims();
    no_exp.as_object_mut().expect("object").remove("exp");
    assert_eq!(verifier().verify(&token(&no_exp)).await.expect_err("no exp").reason(), "missing_claim");
}

/// A refresh token and an ID token are signed by the same realm key as an access token.
#[tokio::test]
async fn a_token_of_the_wrong_type_is_not_an_access_token() {
    for typ in ["Refresh", "ID"] {
        let mut c = admin_claims();
        c["typ"] = json!(typ);
        assert_eq!(verifier().verify(&token(&c)).await.expect_err(typ).reason(), "typ", "a {typ} token was accepted");
    }
}

// ───────────── the organization ─────────────

#[tokio::test]
async fn a_verified_token_with_no_organization_cannot_be_given_a_tenant() {
    let mut c = admin_claims();
    c.as_object_mut().expect("object").remove("organization");
    let err = verifier().verify(&token(&c)).await.expect_err("no organization");
    assert_eq!(err.reason(), "no_organization");
    // 403 and not 401: the token is valid, so refreshing it will not help. The person
    // exists in the realm and belongs to no organization this API can isolate them in.
    assert_eq!(status_of(&state(), get_with("/metrics", Some(&token(&c)))).await, StatusCode::FORBIDDEN);
}

/// The tenant reaches a blob key as `originals/<tenant>/…` and a hash key as a
/// `|`-joined part, so an organization id is checked before it becomes either.
#[tokio::test]
async fn an_organization_id_that_would_leave_its_own_prefix_is_refused() {
    for org in ["../t2", "t1/../t2", "t1|t2", "", "   ", "t 1", "t.1"] {
        let mut c = admin_claims();
        c["organization"] = json!(org);
        let err = verifier().verify(&token(&c)).await.expect_err(org);
        assert_eq!(err.reason(), "no_organization", "{org:?} became a tenant");
    }
}

/// Where the organization lives in the token is an operator's choice of mapper.
#[tokio::test]
async fn the_organization_claim_can_be_nested() {
    let mut cfg = auth_config();
    cfg.org_claim = "koki.org".into();
    let auth = Auth::with_keys(cfg, jwks());

    let mut c = admin_claims();
    c.as_object_mut().expect("object").remove("organization");
    c["koki"] = json!({ "org": "acme" });
    assert_eq!(auth.verify(&token(&c)).await.expect("verify").tenant.as_str(), "acme");
}

// ───────────── the keys ─────────────

/// Serves OIDC discovery and a JWKS the test can swap, and counts the fetches.
struct Realm {
    issuer: String,
    jwks: Arc<Mutex<Option<String>>>,
    fetches: Arc<Mutex<u32>>,
}

async fn realm(initial: Option<&str>) -> Realm {
    let jwks = Arc::new(Mutex::new(initial.map(str::to_string)));
    let fetches = Arc::new(Mutex::new(0u32));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let issuer = format!("http://127.0.0.1:{port}/realms/koki");

    let (j, f, iss) = (jwks.clone(), fetches.clone(), issuer.clone());
    let liar = issuer.clone();
    let app = axum::Router::new()
        // Answers discovery, but names an issuer that is not the one asked for.
        .route(
            "/realms/liar/.well-known/openid-configuration",
            get(move || {
                let liar = liar.clone();
                async move {
                    axum::Json(json!({ "issuer": liar, "jwks_uri": format!("{liar}/protocol/openid-connect/certs") }))
                }
            }),
        )
        .route(
            "/realms/koki/.well-known/openid-configuration",
            get(move || {
                let iss = iss.clone();
                async move {
                    axum::Json(json!({ "issuer": iss, "jwks_uri": format!("{iss}/protocol/openid-connect/certs") }))
                }
            }),
        )
        .route(
            "/realms/koki/protocol/openid-connect/certs",
            get(move || {
                let (j, f) = (j.clone(), f.clone());
                async move {
                    *f.lock().expect("lock") += 1;
                    match j.lock().expect("lock").clone() {
                        Some(body) => ([("content-type", "application/json")], body).into_response(),
                        // A realm that is up but cannot serve its keys.
                        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                }
            }),
        );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Realm { issuer, jwks, fetches }
}

fn config_for(realm: &Realm) -> AuthConfig {
    AuthConfig { issuer: realm.issuer.clone(), ..auth_config() }
}

/// Claims for a realm served on a local port, whose issuer is its address.
fn claims_for(realm: &Realm) -> Value {
    let mut c = admin_claims();
    c["iss"] = json!(realm.issuer);
    c
}

#[tokio::test]
async fn the_keys_are_found_by_discovery_and_a_rotation_is_picked_up_without_a_restart() {
    let realm = realm(Some(JWKS_1)).await;
    let auth = Auth::discover(config_for(&realm)).await.expect("discover");
    assert_eq!(*realm.fetches.lock().expect("lock"), 1, "the keys are fetched once at startup, not per request");

    let valid = token(&claims_for(&realm));
    assert!(auth.verify(&valid).await.is_ok(), "a token from the discovered realm was refused");
    assert_eq!(*realm.fetches.lock().expect("lock"), 1, "a cached key was re-fetched");

    // The realm rotates: the old key is gone, a new one signs. The token's `kid` is now
    // unknown, which is indistinguishable from a forged one, so the first attempt is
    // rate-limited rather than fetching — otherwise anyone could turn this endpoint into
    // a request amplifier against Keycloak with a stream of junk tokens.
    *realm.jwks.lock().expect("lock") = Some(JWKS_2.to_string());
    let rotated = sign_with(&claims_for(&realm), "test-key-2", Algorithm::RS256, EncodingKey::from_rsa_pem(KEY_2).expect("key 2"));
    assert!(auth.verify(&rotated).await.is_err(), "an unknown kid verified against a stale key set");
    assert_eq!(*realm.fetches.lock().expect("lock"), 1, "the rate limit did not hold");

    // Once the limit has passed, the same unknown `kid` provokes exactly one fetch, and
    // the rotated key then verifies — no restart, no configuration change.
    auth.forget_last_fetch_attempt().await;
    assert!(auth.verify(&rotated).await.is_ok(), "a rotated key was not picked up");
    assert_eq!(*realm.fetches.lock().expect("lock"), 2);

    // And the old key is gone, so a token signed with it stops working.
    assert!(auth.verify(&valid).await.is_err(), "a withdrawn key still verifies");
}

/// Keycloak being unreachable is not the caller's fault.
#[tokio::test]
async fn a_realm_that_cannot_serve_its_keys_answers_503_and_recovers() {
    let realm = realm(None).await;
    // Discovery answers, the key fetch does not: startup continues, because a 503 that
    // clears itself is better than a process that will not start while SSO restarts.
    let auth = Auth::discover(config_for(&realm)).await.expect("discovery answered");

    let t = token(&claims_for(&realm));
    let err = auth.verify(&t).await.expect_err("no keys");
    assert_eq!(err.reason(), "keys_unavailable", "{err:?}");
    assert_eq!(
        AuthError::KeysUnavailable.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a missing key set must not look like a bad token"
    );

    *realm.jwks.lock().expect("lock") = Some(JWKS_1.to_string());
    auth.forget_last_fetch_attempt().await;
    assert!(auth.verify(&t).await.is_ok(), "the verifier did not recover once the keys appeared");
}

#[tokio::test]
async fn a_discovery_document_for_a_different_issuer_fails_startup() {
    let realm = realm(Some(JWKS_1)).await;
    // Discovery answers — this is not a 404 — but the document names a realm other than
    // the one configured. Its `jwks_uri` is then the last thing to trust: it names the
    // keys that would be accepted for every request afterwards.
    let mut cfg = config_for(&realm);
    cfg.issuer = realm.issuer.replace("/realms/koki", "/realms/liar");
    let err = Auth::discover(cfg).await.expect_err("a mismatched discovery document was accepted");
    assert!(err.to_string().contains("does not match"), "{err}");
}

/// The development realm and this verifier agree.
///
/// Everything above signs its own tokens, which proves the verifier self-consistent and
/// nothing about whether a *Keycloak* token passes it. This one asks the realm in
/// `deploy/keycloak/realm-dev.json` for a token and verifies that: the audience mapper
/// is really there, the organization claim really arrives, the service account really
/// holds the admin role, and a second organization's token really is a different tenant.
/// A realm file and a verifier can drift apart silently, and this is what notices.
#[tokio::test]
#[ignore = "needs Keycloak from `devenv --profile stack up`"]
async fn a_token_from_the_development_realm_verifies() {
    let issuer = std::env::var("VS_KEYCLOAK_ISSUER").unwrap_or_else(|_| "http://127.0.0.1:8081/realms/koki".into());
    let auth = Auth::discover(AuthConfig { issuer: issuer.clone(), ..auth_config() }).await.expect("discover the dev realm");

    let admin = dev_realm_token(&issuer, "vs-api", "vs-api-dev-secret").await;
    let p = auth.verify(&admin).await.expect("the dev realm's own token was refused");
    assert_eq!(p.tenant.as_str(), "t1", "the organization claim did not arrive as the tenant");
    assert_eq!(p.department.as_deref(), Some("dept-a"));
    assert_eq!(p.workspaces, vec!["ws-red", "ws-blue"], "the multi-valued claim did not arrive as a list");
    assert!(p.admin, "the vs-api service account is missing the {ADMIN_ROLE} realm role");

    // The second client is the same realm and the same key, and must come out as another
    // tenant with no administrative rights.
    let other = dev_realm_token(&issuer, "vs-api-t2", "vs-api-t2-dev-secret").await;
    let p2 = auth.verify(&other).await.expect("the second organization's token was refused");
    assert_eq!(p2.tenant.as_str(), "t2");
    assert!(!p2.admin, "a client with no role mapping came out as an admin");
    assert_ne!(p.subject, p2.subject);
}

/// A client-credentials token from a running Keycloak.
async fn dev_realm_token(issuer: &str, client_id: &str, secret: &str) -> String {
    let res = reqwest::Client::new()
        .post(format!("{issuer}/protocol/openid-connect/token"))
        .form(&[("grant_type", "client_credentials"), ("client_id", client_id), ("client_secret", secret)])
        .send()
        .await
        .unwrap_or_else(|e| panic!("asking {issuer} for a {client_id} token: {e}"));
    let status = res.status();
    let body: Value = res.json().await.expect("token response");
    body["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("no access_token for {client_id} ({status}): {body}"))
        .to_string()
}

/// Telemetry carries no user-specific identifier.
///
/// GDPR, and not a style rule: spans go to a collector and logs get shipped, both with
/// their own retention and their own readers, and a Keycloak `sub` in them is personal
/// data in a store that was never designed to hold any. The organization *is* there, on
/// purpose — it identifies a tenant, not a person, and without it an operator cannot tell
/// whose work a trace belongs to.
///
/// Asserted by capturing what a subscriber is handed during real requests, rather than by
/// reading the span macro: the field was set with `Span::record` from the authentication
/// middleware, which no amount of looking at `info_span!` would show. The subscriber is
/// global and installed once, so this also watches every *other* test in this binary —
/// anything any of them puts in a span or an event is held to the same rule.
#[test]
fn telemetry_carries_no_user_specific_identifier() {
    let seen = capturing_subscriber();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    rt.block_on(async {
        let admin = token(&admin_claims());
        let res = router(state()).oneshot(get_with("/metrics", Some(&admin))).await.expect("router");
        assert_eq!(res.status(), StatusCode::OK);
        // The refusal path too: it used to log who was refused.
        let plain = token(&claims());
        let res = router(state()).oneshot(get_with("/metrics", Some(&plain))).await.expect("router");
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    });

    let captured = seen.lock().expect("lock").join("\n");
    assert!(captured.contains("http_request"), "nothing was captured, so this test proves nothing:\n{captured}");
    assert!(
        !captured.contains(SUBJECT),
        "the subject reached telemetry — a user-specific identifier in a span or log line:\n{captured}"
    );
    assert!(!captured.contains("11111111-1111"), "the token id (jti) reached telemetry:\n{captured}");
    // The tenant is deliberately present.
    assert!(captured.contains(ORG), "the organization is missing, so a trace cannot be attributed to a tenant:\n{captured}");

    // And a principal cannot leak one by being printed, which is how it would come back:
    // the subject is redacted in `Debug`.
    let p = rt.block_on(async { verifier().verify(&token(&admin_claims())).await.expect("verify") });
    let printed = format!("{p:?}");
    assert!(printed.contains("<redacted>"), "{printed}");
    assert!(!printed.contains(SUBJECT), "Debug printed the subject: {printed}");
    let owner = format!("{:?}", p.owner());
    assert!(!owner.contains(SUBJECT), "Debug of the job's owner printed the subject: {owner}");
}

/// Installs a global capturing subscriber once and returns its sink.
///
/// Global rather than scoped with `with_default`, because `tracing` caches each callsite's
/// interest against the *global* dispatcher: the sibling tests drive the same router, and
/// whichever of them reaches `http_request` first would otherwise cache it as "of
/// interest to nobody" and leave this test capturing nothing.
fn capturing_subscriber() -> Arc<Mutex<Vec<String>>> {
    use tracing_subscriber::layer::SubscriberExt;

    static SINK: std::sync::OnceLock<Arc<Mutex<Vec<String>>>> = std::sync::OnceLock::new();
    SINK.get_or_init(|| {
        let sink: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureFields(sink.clone()));
        tracing::subscriber::set_global_default(subscriber).expect("nothing else installs a subscriber here");
        tracing::callsite::rebuild_interest_cache();
        sink
    })
    .clone()
}

/// Records every span field and event field a subscriber is handed.
struct CaptureFields(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureFields {
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, _: &tracing::span::Id, _: tracing_subscriber::layer::Context<'_, S>) {
        self.0.lock().expect("lock").push(attrs.metadata().name().to_string());
        attrs.record(&mut Collect(&self.0));
    }
    fn on_record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        values.record(&mut Collect(&self.0));
    }
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        event.record(&mut Collect(&self.0));
    }
}

struct Collect<'a>(&'a Arc<Mutex<Vec<String>>>);

impl tracing::field::Visit for Collect<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.lock().expect("lock").push(format!("{}={value:?}", field.name()));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.lock().expect("lock").push(format!("{}={value}", field.name()));
    }
}

#[test]
fn a_config_that_would_weaken_verification_is_refused() {
    for (field, cfg) in [
        ("issuer", AuthConfig { issuer: String::new(), ..auth_config() }),
        ("relative issuer", AuthConfig { issuer: "sso.test/realms/koki".into(), ..auth_config() }),
        ("audience", AuthConfig { audience: vec![], ..auth_config() }),
        ("blank audience", AuthConfig { audience: vec!["  ".into()], ..auth_config() }),
        ("admin_role", AuthConfig { admin_role: String::new(), ..auth_config() }),
    ] {
        assert!(cfg.validate().is_err(), "{field} was accepted");
    }
    assert!(auth_config().validate().is_ok());
}

/// Base64url-decodes a JWK component, for the HMAC-confusion test's second shape.
fn base64_url(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap_or_else(|_| s.as_bytes().to_vec())
}

use axum::response::IntoResponse;
