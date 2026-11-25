//! Authentication: Keycloak-issued OIDC access tokens.
//!
//! One verification point. `authenticate` is a middleware over every route that is not
//! `/healthz` or `/readyz`, and it is the only thing that constructs a [`Principal`];
//! the extractors below read what it put in the request extensions and cannot be
//! satisfied any other way. A route added outside that layer therefore fails closed —
//! `Tenant` answers 500, not "no tenant" — and `every_v1_route_requires_a_token` in
//! `auth_tests.rs` is what notices.
//!
//! **The tenant is the organization.** It comes from a verified claim, never from a
//! header or a body, and it is the isolation boundary: it is hashed into every document
//! and entity key, prefixes every blob key, and partitions the Qdrant collections.
//!
//! The rest of the caller's identity — person, department, workspaces — is recorded on the
//! job as a [`ds_core::Owner`] **and enforced**: see [`crate::scope`] for the read rule
//! and `SPEC.md` for what each check is for. This module's job ends at establishing who
//! the caller is, and now also at refusing a token that has been revoked since it was
//! issued.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, request::Parts, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

use ds_core::{Owner, TenantId};
use ds_infra::config::AuthConfig;
use ds_vault::access::{Revocation, RevocationKind};
use ds_vault::ops::Ops;

/// Signature algorithms this API will verify.
///
/// Asymmetric only, and leaving `HS*` out is the point rather than an oversight: the
/// verification keys come from a *public* JWKS, so accepting an HMAC algorithm would let
/// anyone who can read that document sign their own tokens, using the public modulus as
/// the shared secret. `jsonwebtoken` also refuses the key-family mismatch on its own, so
/// this is the first of two defences, not the only one — `a_token_signed_with_the_public_key_as_an_hmac_secret_is_refused`
/// holds both of them to it.
const ALLOWED_ALGS: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

/// Minimum spacing between JWKS fetches provoked by an unknown `kid`.
///
/// An unknown `kid` is either a key rotation or a forged token, and the second kind
/// arrives as fast as an attacker can send it. Without this floor, an unauthenticated
/// request would be an amplifier: one junk token, one fetch against Keycloak.
const MIN_REFETCH: Duration = Duration::from_secs(10);

/// Spec claims required in every token, beyond a valid signature.
const REQUIRED_CLAIMS: [&str; 4] = ["exp", "iss", "aud", "sub"];

/// The event a back-channel logout token must carry (OpenID Connect Back-Channel
/// Logout 1.0 §2.4).
const LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

/// How old a logout token may be. The specification says a logout token is short-lived
/// without putting a number on it; this is the replay window for one that is captured
/// in flight, and five minutes is the same order as the clock skew everything else here
/// allows. A replayed token inside the window is harmless anyway — it revokes a session
/// that is already revoked.
const MAX_LOGOUT_TOKEN_AGE: Duration = Duration::from_secs(300);

/// How long a revocation is kept after it is recorded.
///
/// A revocation only has to outlive the tokens it refuses, and an access token's
/// lifetime is the realm's business — minutes, by default, in Keycloak. A day is far
/// past any sane access-token lifetime and still bounds the table, and `purge_revocations`
/// is what reclaims it.
pub const REVOCATION_TTL: chrono::TimeDelta = chrono::TimeDelta::days(1);

/// Longest accepted organization id. It is a blob-key prefix and a hash-key part, so it
/// is bounded here rather than wherever it is first concatenated.
const MAX_TENANT_LEN: usize = 64;

/// A verified caller.
#[derive(Clone, PartialEq, Eq)]
pub struct Principal {
    /// Keycloak `sub`: stable, and not an email address.
    ///
    /// **Not for telemetry.** It is a user-specific identifier, so it belongs in the job
    /// row that needs it for access control and nowhere else — not in a span, not in a
    /// log line, not in a metric label (`docs/SPEC.md` §5.6). `Debug` redacts it for that
    /// reason: a principal is one `?principal` away from a log line, and a rule that
    /// depends on everyone remembering it is not a rule.
    pub subject: String,
    /// The organization, which is the tenant and the isolation boundary.
    pub tenant: TenantId,
    pub department: Option<String>,
    pub workspaces: Vec<String>,
    /// Realm roles plus, when `auth.client_id` is set, that client's roles.
    pub roles: Vec<String>,
    /// Whether `roles` contains `auth.admin_role`, resolved once here so the admin
    /// extractor does not need the config.
    pub admin: bool,
    /// `jti`, for correlating a request with the token it used.
    pub token_id: Option<String>,
    /// Keycloak's `sid`: the session this token belongs to. What a back-channel logout
    /// names, and therefore what makes revocation possible at all for a token that
    /// carries no `jti`.
    pub session_id: Option<String>,
    /// `iat`, as seconds since the epoch. Read because a subject- or session-wide
    /// revocation is "every token issued before this instant", so the question is when
    /// this one was issued.
    pub issued_at: Option<i64>,
}

impl std::fmt::Debug for Principal {
    /// Everything but the subject, which is redacted — see `subject`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Principal")
            .field("subject", &"<redacted>")
            .field("tenant", &self.tenant.as_str())
            .field("department", &self.department)
            .field("workspaces", &self.workspaces)
            .field("roles", &self.roles)
            .field("admin", &self.admin)
            .finish()
    }
}

impl Principal {
    /// What gets recorded on the job.
    pub fn owner(&self) -> Owner {
        Owner {
            subject: self.subject.clone(),
            department: self.department.clone(),
            workspaces: self.workspaces.clone(),
        }
    }

    /// What read scoping is evaluated against.
    ///
    /// `admin_reads_all` comes from the configuration rather than from the role alone,
    /// because whether administration implies access is a deployment's decision and not
    /// this crate's (`api.admin_reads_all`).
    pub fn reader(&self, admin_reads_all: bool) -> ds_vault::access::Reader {
        ds_vault::access::Reader {
            subject: self.subject.clone(),
            department: self.department.clone(),
            workspaces: self.workspaces.clone(),
            admin: self.admin && admin_reads_all,
        }
    }
}

/// Why a request was refused. The caller is told the class, never the detail: a precise
/// message about *which* check failed is a probing oracle. The detail goes to the log
/// and to the `reason` metric label.
#[derive(Debug)]
pub enum AuthError {
    /// No bearer token at all.
    Missing,
    /// Structurally wrong, or using an algorithm or key this API will not accept.
    Malformed(&'static str),
    /// Correctly shaped, but signed by a key the realm does not publish.
    UnknownKey,
    /// Signature, expiry, issuer or audience check failed.
    Invalid(jsonwebtoken::errors::ErrorKind),
    /// Verified, but carries no organization — authentication without entitlement.
    NoOrganization,
    /// Verified and unexpired, but the session, the token or the person has been
    /// revoked since it was issued.
    Revoked,
    /// Verified, but missing the role the route needs.
    MissingRole,
    /// No verification keys available: Keycloak was unreachable. Our fault, not the
    /// caller's, so it is a 503 and not a 401 — a client that retries is right to.
    KeysUnavailable,
    /// A route outside the authentication layer. A bug in `router`, not a request.
    NotAuthenticated,
}

impl AuthError {
    /// Stable label for logs and the `ds_api_auth_failures_total` metric.
    pub fn reason(&self) -> &'static str {
        match self {
            AuthError::Missing => "missing",
            AuthError::Malformed(what) => what,
            AuthError::UnknownKey => "unknown_key",
            AuthError::Invalid(kind) => match kind {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => "expired",
                jsonwebtoken::errors::ErrorKind::InvalidIssuer => "issuer",
                jsonwebtoken::errors::ErrorKind::InvalidAudience => "audience",
                jsonwebtoken::errors::ErrorKind::InvalidSignature => "signature",
                jsonwebtoken::errors::ErrorKind::ImmatureSignature => "not_yet_valid",
                jsonwebtoken::errors::ErrorKind::MissingRequiredClaim(_) => "missing_claim",
                _ => "invalid",
            },
            AuthError::NoOrganization => "no_organization",
            AuthError::Revoked => "revoked",
            AuthError::MissingRole => "missing_role",
            AuthError::KeysUnavailable => "keys_unavailable",
            AuthError::NotAuthenticated => "route_not_authenticated",
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self {
            AuthError::NoOrganization | AuthError::MissingRole => StatusCode::FORBIDDEN,
            AuthError::KeysUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            AuthError::NotAuthenticated => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNAUTHORIZED,
        }
    }

    /// What the caller is told. Deliberately coarse.
    fn message(&self) -> &'static str {
        match self {
            AuthError::Missing => "missing bearer token",
            AuthError::Revoked => "token revoked",
            AuthError::NoOrganization => "token carries no organization",
            AuthError::MissingRole => "this route needs an administrative role",
            AuthError::KeysUnavailable => "verification keys unavailable",
            AuthError::NotAuthenticated => "internal error",
            _ => "invalid token",
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let reason = self.reason();
        metrics::counter!("ds_api_auth_failures_total", "reason" => reason).increment(1);
        if matches!(self, AuthError::NotAuthenticated) {
            tracing::error!("a route is served outside the authentication layer");
        } else {
            tracing::debug!(reason, "request not authenticated");
        }
        let status = self.status();
        let mut res = (status, self.message().to_string()).into_response();
        if status == StatusCode::UNAUTHORIZED {
            // RFC 6750: say it is a bearer scheme and which error class, so a client
            // library knows to refresh rather than to re-prompt.
            let v = if matches!(self, AuthError::Missing) {
                "Bearer"
            } else {
                "Bearer error=\"invalid_token\""
            };
            res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(v));
        }
        if status == StatusCode::SERVICE_UNAVAILABLE {
            res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        }
        res
    }
}

/// What a verified back-channel logout token asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutRequest {
    /// The session to end, if the token named one.
    pub session_id: Option<String>,
    /// The person whose tokens to refuse, if the token named one.
    pub subject: Option<String>,
    /// The organization, when the token carries the claim. Logout is the realm's to
    /// order, so a token without it still logs the session out.
    pub tenant: Option<TenantId>,
}

/// The subset of the OIDC discovery document this needs.
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

struct Cache {
    jwks: JwkSet,
    /// When a fetch last succeeded. `None` with a remote key source means no key has
    /// ever been available.
    fetched_at: Option<Instant>,
    /// When a fetch was last *attempted*, successful or not; the rate-limit clock.
    last_attempt: Option<Instant>,
}

impl Cache {
    /// No keys yet. `JwkSet` has no `Default`.
    fn empty() -> Self {
        Self { jwks: JwkSet { keys: Vec::new() }, fetched_at: None, last_attempt: None }
    }
}

/// Verifies tokens against a realm's published keys.
pub struct Auth {
    cfg: AuthConfig,
    http: reqwest::Client,
    /// `None` when the key set was supplied directly, which is how the tests avoid
    /// needing a Keycloak. The fetch path has its own test against a real socket.
    jwks_uri: Option<String>,
    keys: RwLock<Cache>,
    /// Tokens that must stop being accepted before they expire.
    ///
    /// Held in memory and refreshed on a timer rather than read per request: a signature
    /// is verified locally precisely so that Keycloak — and now Postgres — is not on the
    /// path of every call (D10). `api.revocation_refresh_s` is therefore the window in
    /// which a revoked token still works on a copy that has not refreshed yet, and it is
    /// a stated window rather than a hidden one.
    revocations: RwLock<Revocations>,
    /// Where the list comes from. Unset disables revocation entirely, which is what a
    /// test with no database gets. A `OnceLock` because it is set once, after
    /// `discover` has already handed the verifier out as an `Arc`, and is then only
    /// ever read.
    ops: std::sync::OnceLock<Arc<Ops>>,
}

/// The revocation list, indexed the three ways it is asked.
#[derive(Debug, Default)]
pub struct Revocations {
    /// Single tokens, by `jti`. No instant: the token itself is revoked.
    jti: std::collections::HashSet<String>,
    /// Sessions, by `sid`. No instant either: a session is ended as a whole, and a
    /// person who logs in again gets a new `sid` rather than a newer token under the old
    /// one, so there is nothing an instant would usefully separate.
    sid: std::collections::HashSet<String>,
    /// People, by `sub` → tokens issued before this are refused.
    subject: std::collections::HashMap<String, i64>,
}

impl Revocations {
    fn from_rows(rows: Vec<Revocation>) -> Self {
        let mut r = Revocations::default();
        for row in rows {
            match row.kind {
                RevocationKind::Jti => {
                    r.jti.insert(row.id);
                }
                RevocationKind::Sid => {
                    r.sid.insert(row.id);
                }
                RevocationKind::Subject => {
                    let e = r.subject.entry(row.id).or_insert(i64::MIN);
                    *e = (*e).max(row.not_before.timestamp());
                }
            }
        }
        r
    }

    fn len(&self) -> usize {
        self.jti.len() + self.sid.len() + self.subject.len()
    }

    /// Whether this token has been revoked.
    ///
    /// A `jti` or a `sid` is absolute: that token, or that session, is finished. A
    /// *subject* revocation is "every token issued before this instant", because "log me
    /// out everywhere" must not refuse the login the person makes a second later.
    ///
    /// A token with no `iat` is refused by a subject revocation rather than let through:
    /// the question it asks cannot be answered without an issue time, and the safe
    /// reading of an unanswerable question here is the restrictive one. Keycloak always
    /// issues `iat`, so a token without one is the forged case.
    pub fn is_revoked(&self, p: &Principal) -> bool {
        if p.token_id.as_ref().is_some_and(|j| self.jti.contains(j)) {
            return true;
        }
        if p.session_id.as_ref().is_some_and(|s| self.sid.contains(s)) {
            return true;
        }
        self.subject.get(&p.subject).copied().is_some_and(|not_before| p.issued_at.is_none_or(|iat| iat < not_before))
    }
}

impl std::fmt::Debug for Auth {
    /// The issuer and where the keys come from. Not the keys: they are public, but a
    /// verifier dumped into a log line is noise either way.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").field("issuer", &self.cfg.issuer).field("jwks_uri", &self.jwks_uri).finish_non_exhaustive()
    }
}

impl Auth {
    /// Reads the realm's discovery document, then its keys.
    ///
    /// Discovery is retried a few times because a rolling deploy can land while Keycloak
    /// is still coming up, and then fails the process: a misconfigured issuer must not
    /// come up as a working API. A *key* fetch failing afterwards does not fail startup —
    /// requests answer 503 until one succeeds, which is recoverable without a restart.
    pub async fn discover(cfg: AuthConfig) -> anyhow::Result<Arc<Self>> {
        cfg.validate()?;
        // `jsonwebtoken` 12 verifies through a pluggable provider, chosen here by
        // enabling exactly one backend feature. Asked for at startup so a build with none
        // — or both — fails now rather than on the first request that needs a signature
        // checked.
        if jsonwebtoken::crypto::CryptoProvider::try_get_default().is_none() {
            anyhow::bail!("no JWT crypto provider: build ds-api with exactly one of jsonwebtoken's `rust_crypto` or `aws_lc_rs` features");
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
        let url = format!("{}/.well-known/openid-configuration", cfg.issuer.trim_end_matches('/'));

        let mut last: Option<anyhow::Error> = None;
        let mut doc = None;
        for attempt in 1..=3u32 {
            match fetch_discovery(&http, &url).await {
                Ok(d) => {
                    doc = Some(d);
                    break;
                }
                Err(e) => {
                    tracing::warn!(attempt, url = %url, error = %e, "OIDC discovery failed");
                    last = Some(e);
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
        let doc = match doc {
            Some(d) => d,
            None => anyhow::bail!("OIDC discovery at {url} failed: {}", last.expect("an error after three failures")),
        };
        // The discovery document names its own issuer. If it disagrees with the
        // configured one, something is answering for a realm that is not ours, and its
        // `jwks_uri` is the last thing to trust.
        if doc.issuer.trim_end_matches('/') != cfg.issuer.trim_end_matches('/') {
            anyhow::bail!("discovery issuer {:?} does not match auth.issuer {:?}", doc.issuer, cfg.issuer);
        }

        let auth = Arc::new(Self {
            cfg,
            http,
            jwks_uri: Some(doc.jwks_uri),
            keys: RwLock::new(Cache::empty()),
            revocations: RwLock::new(Revocations::default()),
            ops: Default::default(),
        });
        if let Err(e) = auth.refresh().await {
            tracing::error!(error = %e, "JWKS prefetch failed; requests answer 503 until a fetch succeeds");
        }
        Ok(auth)
    }

    /// A verifier over a fixed key set, so a test needs no Keycloak. The fetch path is
    /// not skipped by this: it has its own tests against a real socket.
    #[cfg(test)]
    pub fn with_keys(cfg: AuthConfig, jwks: JwkSet) -> Arc<Self> {
        let cache = Cache { jwks, fetched_at: Some(Instant::now()), last_attempt: None };
        Arc::new(Self {
            cfg,
            http: reqwest::Client::new(),
            jwks_uri: None,
            keys: RwLock::new(cache),
            revocations: RwLock::new(Revocations::default()),
            ops: Default::default(),
        })
    }

    /// Gives this verifier the revocation list. Without it every token is good until it
    /// expires, which is what this API did before.
    ///
    /// Set once, during startup, after `discover` has already handed out the `Arc`. A
    /// second call is ignored rather than replacing the source: two revocation lists
    /// behind one verifier is a configuration nobody means.
    pub fn with_revocations(&self, ops: Arc<Ops>) {
        if self.ops.set(ops).is_err() {
            tracing::warn!("revocation source already set; the second one is ignored");
        }
    }

    /// Re-reads the revocation list every `refresh` and purges rows whose tokens have
    /// all expired. Does nothing without [`Auth::with_revocations`].
    pub fn spawn_revocation_refresher(self: &Arc<Self>, refresh: Duration) {
        if self.ops.get().is_none() {
            tracing::warn!("no revocation source configured; tokens stay valid until they expire");
            return;
        }
        let this = Arc::downgrade(self);
        let period = refresh.max(Duration::from_secs(1));
        tokio::spawn(async move {
            // Immediately, so the first request after startup already sees the list.
            if let Some(auth) = this.upgrade() {
                auth.refresh_revocations().await;
            }
            let mut tick = tokio::time::interval(period);
            tick.tick().await;
            let mut since_purge = 0u32;
            loop {
                tick.tick().await;
                let Some(auth) = this.upgrade() else { return };
                auth.refresh_revocations().await;
                // Expired rows match nothing, so purging them is housekeeping rather
                // than correctness: hourly-ish is enough.
                since_purge += 1;
                if since_purge as u64 * period.as_secs() >= 3600 {
                    since_purge = 0;
                    if let Some(ops) = auth.ops.get() {
                        match ops.purge_revocations().await {
                            Ok(n) if n > 0 => tracing::info!(rows = n, "purged expired revocations"),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "could not purge revocations"),
                        }
                    }
                }
            }
        });
    }

    async fn refresh_revocations(&self) {
        let Some(ops) = self.ops.get() else { return };
        match ops.live_revocations().await {
            Ok(rows) => {
                let next = Revocations::from_rows(rows);
                let n = next.len();
                *self.revocations.write().await = next;
                metrics::gauge!("ds_api_revocations").set(n as f64);
            }
            // The cached list is kept: serving the previous one is better than serving
            // none, which would un-revoke everything for as long as Postgres is away.
            Err(e) => tracing::warn!(error = %e, "could not refresh the revocation list; serving the cached one"),
        }
    }

    /// Revokes a token, a session or a person, and applies it to this copy at once.
    ///
    /// The other copies pick it up on their next refresh — `api.revocation_refresh_s`
    /// is that window.
    pub async fn revoke(
        &self,
        kind: RevocationKind,
        id: &str,
        tenant: Option<&TenantId>,
        not_before: chrono::DateTime<chrono::Utc>,
        expires_at: chrono::DateTime<chrono::Utc>,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let ops = self.ops.get().ok_or_else(|| anyhow::anyhow!("no revocation source configured"))?;
        ops.revoke_token(kind, id, tenant, not_before, expires_at, reason).await?;
        self.refresh_revocations().await;
        metrics::counter!("ds_api_revocations_total", "kind" => kind.as_str()).increment(1);
        Ok(())
    }

    /// The live list, for a test and for the status route.
    pub async fn revocation_count(&self) -> usize {
        self.revocations.read().await.len()
    }

    /// Replaces the cached list directly, so a test can exercise the check without a
    /// database behind it.
    #[cfg(test)]
    pub(crate) async fn set_revocations_for_test(&self, rows: Vec<Revocation>) {
        *self.revocations.write().await = Revocations::from_rows(rows);
    }

    /// Re-reads the JWKS every `auth.jwks_refresh_s`, so a key the realm withdrew stops
    /// verifying without waiting for a request that happens to miss the cache.
    pub fn spawn_refresher(self: &Arc<Self>) {
        if self.jwks_uri.is_none() {
            return;
        }
        let this = Arc::downgrade(self);
        let period = Duration::from_secs(self.cfg.jwks_refresh_s.max(30));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.tick().await; // the immediate first tick; `discover` already fetched
            loop {
                tick.tick().await;
                let Some(auth) = this.upgrade() else { return };
                if let Err(e) = auth.refresh().await {
                    tracing::warn!(error = %e, "scheduled JWKS refresh failed; serving the cached keys");
                }
            }
        });
    }

    /// Clears the rate-limit clock, so a test can assert what happens on the next
    /// unknown `kid` without waiting out `MIN_REFETCH`.
    #[cfg(test)]
    pub(crate) async fn forget_last_fetch_attempt(&self) {
        self.keys.write().await.last_attempt = None;
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        let Some(uri) = self.jwks_uri.clone() else {
            anyhow::bail!("this verifier has a fixed key set");
        };
        self.keys.write().await.last_attempt = Some(Instant::now());
        let outcome = fetch_jwks(&self.http, &uri).await;
        match outcome {
            Ok(jwks) => {
                let keys = jwks.keys.len();
                let mut c = self.keys.write().await;
                c.jwks = jwks;
                c.fetched_at = Some(Instant::now());
                drop(c);
                metrics::counter!("ds_api_jwks_fetches_total", "outcome" => "ok").increment(1);
                tracing::info!(keys, uri = %uri, "JWKS refreshed");
                Ok(())
            }
            Err(e) => {
                metrics::counter!("ds_api_jwks_fetches_total", "outcome" => "error").increment(1);
                Err(e)
            }
        }
    }

    /// The key a `kid` names, fetching once if the realm appears to have rotated.
    async fn jwk_for(&self, kid: &str) -> Result<Jwk, AuthError> {
        if let Some(j) = self.keys.read().await.jwks.find(kid) {
            return Ok(j.clone());
        }
        let may_fetch = {
            let c = self.keys.read().await;
            self.jwks_uri.is_some() && c.last_attempt.is_none_or(|t| t.elapsed() >= MIN_REFETCH)
        };
        if may_fetch {
            if let Err(e) = self.refresh().await {
                tracing::warn!(error = %e, "JWKS refresh for an unknown kid failed");
            }
        }
        let c = self.keys.read().await;
        if let Some(j) = c.jwks.find(kid) {
            return Ok(j.clone());
        }
        if self.jwks_uri.is_some() && c.fetched_at.is_none() {
            return Err(AuthError::KeysUnavailable);
        }
        Err(AuthError::UnknownKey)
    }

    /// Verifies a bearer token and resolves the caller.
    pub async fn verify(&self, token: &str) -> Result<Principal, AuthError> {
        let header = decode_header(token).map_err(|_| AuthError::Malformed("header"))?;
        if !ALLOWED_ALGS.contains(&header.alg) {
            return Err(AuthError::Malformed("alg"));
        }
        let kid = header.kid.ok_or(AuthError::Malformed("kid"))?;
        let jwk = self.jwk_for(&kid).await?;
        // A key that declares its algorithm is held to it: the realm said what this key
        // is for, and a header asking for something else is not it.
        if let Some(declared) = declared_alg(&jwk) {
            if declared != header.alg {
                return Err(AuthError::Malformed("alg_not_the_keys"));
            }
        }
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthError::Malformed("jwk"))?;

        let mut v = Validation::new(header.alg);
        v.algorithms = vec![header.alg];
        v.leeway = self.cfg.leeway_s;
        // Off by default in `jsonwebtoken`, so a token that is not valid yet would pass.
        v.validate_nbf = true;
        v.set_issuer(&[self.cfg.issuer.as_str()]);
        v.set_audience(&self.cfg.audience);
        v.set_required_spec_claims(&REQUIRED_CLAIMS);

        let data = decode::<Value>(token, &key, &v).map_err(|e| AuthError::Invalid(e.into_kind()))?;
        let principal = self.principal(&data.claims)?;
        // After the signature and the claims, because revoking something that was never
        // valid is not a thing to spend a lock on.
        if self.revocations.read().await.is_revoked(&principal) {
            return Err(AuthError::Revoked);
        }
        Ok(principal)
    }

    /// Verifies a back-channel logout token from the realm (OpenID Connect Back-Channel
    /// Logout 1.0).
    ///
    /// This is what makes revocation something the identity provider can order rather
    /// than something an operator has to remember: Keycloak posts one of these when a
    /// session ends, and the session's still-unexpired access tokens stop being accepted.
    ///
    /// It is held to the same key set and issuer as an access token, and then to the
    /// three things that distinguish it from one: the `events` claim naming the logout
    /// event, no `nonce` (§2.4 forbids it, because an ID token with one would otherwise
    /// pass here), and a `sid` or a `sub` to act on. It must *not* satisfy the access-token
    /// path, and `an_id_token_is_not_a_logout_token` is what holds it to that.
    pub async fn verify_logout_token(&self, token: &str) -> Result<LogoutRequest, AuthError> {
        let header = decode_header(token).map_err(|_| AuthError::Malformed("header"))?;
        if !ALLOWED_ALGS.contains(&header.alg) {
            return Err(AuthError::Malformed("alg"));
        }
        let kid = header.kid.ok_or(AuthError::Malformed("kid"))?;
        let jwk = self.jwk_for(&kid).await?;
        if let Some(declared) = declared_alg(&jwk) {
            if declared != header.alg {
                return Err(AuthError::Malformed("alg_not_the_keys"));
            }
        }
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| AuthError::Malformed("jwk"))?;

        let mut v = Validation::new(header.alg);
        v.algorithms = vec![header.alg];
        v.leeway = self.cfg.leeway_s;
        v.validate_nbf = true;
        v.set_issuer(&[self.cfg.issuer.as_str()]);
        v.set_audience(&self.cfg.audience);
        // A logout token carries `iat` and `jti` and need not carry `exp` or `sub`
        // (§2.4), so freshness is checked from `iat` below rather than left to `exp`.
        v.set_required_spec_claims(&["iss", "aud"]);
        let claims = decode::<Value>(token, &key, &v).map_err(|e| AuthError::Invalid(e.into_kind()))?.claims;

        // §2.6: a token with a `nonce` is an ID token, and accepting one here would turn
        // every login into a logout.
        if claims.get("nonce").is_some() {
            return Err(AuthError::Malformed("logout_nonce"));
        }
        let has_event = claims
            .get("events")
            .and_then(Value::as_object)
            .is_some_and(|e| e.contains_key(LOGOUT_EVENT));
        if !has_event {
            return Err(AuthError::Malformed("logout_events"));
        }
        let iat = claims.get("iat").and_then(Value::as_i64).ok_or(AuthError::Malformed("logout_iat"))?;
        let age = chrono::Utc::now().timestamp() - iat;
        if age > MAX_LOGOUT_TOKEN_AGE.as_secs() as i64 || age < -(self.cfg.leeway_s as i64) {
            return Err(AuthError::Malformed("logout_stale"));
        }
        let session_id = claims.get("sid").and_then(Value::as_str).map(str::to_string);
        let subject = claims.get("sub").and_then(Value::as_str).map(str::to_string);
        if session_id.is_none() && subject.is_none() {
            return Err(AuthError::Malformed("logout_target"));
        }
        Ok(LogoutRequest {
            session_id,
            subject,
            tenant: claim_str(&claims, &self.cfg.org_claim).as_deref().and_then(tenant_from),
        })
    }

    /// Acts on a verified logout token: the session, or the person, stops being accepted.
    ///
    /// A `sid` revokes that session; a token with only a `sub` revokes every token of
    /// that person issued before now, which is what "the user logged out everywhere"
    /// means. `not_before` is the logout instant, so a *new* login right afterwards is
    /// unaffected.
    pub async fn apply_logout(&self, req: &LogoutRequest) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let expires = now + REVOCATION_TTL;
        if let Some(sid) = &req.session_id {
            self.revoke(RevocationKind::Sid, sid, req.tenant.as_ref(), now, expires, Some("backchannel logout")).await?;
        } else if let Some(sub) = &req.subject {
            self.revoke(RevocationKind::Subject, sub, req.tenant.as_ref(), now, expires, Some("backchannel logout")).await?;
        }
        Ok(())
    }

    fn principal(&self, claims: &Value) -> Result<Principal, AuthError> {
        // Keycloak marks an access token `typ: "Bearer"`. A refresh or ID token from the
        // same realm is signed by the same key and would otherwise verify here.
        if let Some(t) = claims.get("typ").and_then(Value::as_str) {
            if !t.eq_ignore_ascii_case("bearer") {
                return Err(AuthError::Malformed("typ"));
            }
        }
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(AuthError::Malformed("sub"))?
            .to_string();

        let tenant = claim_str(claims, &self.cfg.org_claim)
            .as_deref()
            .and_then(tenant_from)
            .ok_or(AuthError::NoOrganization)?;

        let roles = roles_from(claims, self.cfg.client_id.as_deref());
        let admin = roles.iter().any(|r| r == &self.cfg.admin_role);

        Ok(Principal {
            subject,
            tenant,
            department: claim_str(claims, &self.cfg.department_claim),
            workspaces: claim_strings(claims, &self.cfg.workspaces_claim),
            roles,
            admin,
            token_id: claims.get("jti").and_then(Value::as_str).map(str::to_string),
            session_id: claims.get("sid").and_then(Value::as_str).map(str::to_string),
            issued_at: claims.get("iat").and_then(Value::as_i64),
        })
    }
}

async fn fetch_discovery(http: &reqwest::Client, url: &str) -> anyhow::Result<Discovery> {
    Ok(http.get(url).send().await?.error_for_status()?.json::<Discovery>().await?)
}

async fn fetch_jwks(http: &reqwest::Client, uri: &str) -> anyhow::Result<JwkSet> {
    let jwks: JwkSet = http.get(uri).send().await?.error_for_status()?.json().await?;
    if jwks.keys.is_empty() {
        anyhow::bail!("JWKS at {uri} published no keys");
    }
    Ok(jwks)
}

/// The algorithm a JWK declares, if it declares one.
fn declared_alg(jwk: &Jwk) -> Option<Algorithm> {
    let ka = jwk.common.key_algorithm?;
    // `KeyAlgorithm` and `Algorithm` are separate enums with the same JOSE names.
    Algorithm::from_str(&ka.to_string()).ok()
}

/// Accepts an organization id, or nothing.
///
/// The charset is not cosmetic: this value is a path segment in every blob key
/// (`originals/<tenant>/…`) and a `|`-joined part of every hash key, so a `/`, a `..` or
/// a `|` here would be a path traversal or a key collision between two organizations.
fn tenant_from(raw: &str) -> Option<TenantId> {
    let t = raw.trim();
    let ok = !t.is_empty()
        && t.len() <= MAX_TENANT_LEN
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| TenantId(t.to_string()))
}

/// Reads a claim by a dotted path, so a mapper that nests (`koki.org`) needs no code.
fn claim_at<'a>(claims: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = claims;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        cur = cur.get(segment)?;
    }
    Some(cur)
}

fn claim_str(claims: &Value, path: &str) -> Option<String> {
    let v = claim_at(claims, path)?;
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        // Keycloak's organization mapper can emit a single-element array.
        Value::Array(a) if a.len() == 1 => a[0].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// An array of strings, or one space-separated string (the shape `scope` uses).
fn claim_strings(claims: &Value, path: &str) -> Vec<String> {
    match claim_at(claims, path) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        Some(Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// Realm roles, plus one client's roles when `auth.client_id` names one.
fn roles_from(claims: &Value, client_id: Option<&str>) -> Vec<String> {
    let mut roles = claim_strings(claims, "realm_access.roles");
    if let Some(client) = client_id {
        // Indexed directly rather than through `claim_at`, because a client id may
        // contain a `.` and would then be read as a path.
        let client_roles = claims
            .get("resource_access")
            .and_then(|r| r.get(client))
            .and_then(|c| c.get("roles"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        roles.extend(client_roles);
    }
    roles.sort_unstable();
    roles.dedup();
    roles
}

// ───────────── middleware and extractors ─────────────

/// Verifies the bearer token and attaches the [`Principal`]. The only place a principal
/// is made, and the only place a token is read.
pub async fn authenticate(State(auth): State<Arc<Auth>>, mut req: Request, next: Next) -> Response {
    let token = match bearer(&req) {
        Some(t) => t,
        None => return AuthError::Missing.into_response(),
    };
    match auth.verify(&token).await {
        Ok(principal) => {
            // The organization, on the request's span, so every log line under it says
            // which tenant's work this was. Not the subject: see `Principal::subject`.
            tracing::Span::current().record("tenant", principal.tenant.as_str());
            req.extensions_mut().insert(Arc::new(principal));
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

fn bearer(req: &Request) -> Option<String> {
    let v = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn principal_of(parts: &Parts) -> Result<Arc<Principal>, AuthError> {
    parts.extensions.get::<Arc<Principal>>().cloned().ok_or(AuthError::NotAuthenticated)
}

/// The calling organization: the tenant, and the isolation boundary.
pub struct Tenant(pub TenantId);

impl<S: Send + Sync> FromRequestParts<S> for Tenant {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Tenant(principal_of(parts)?.tenant.clone()))
    }
}

/// The whole verified caller, for the routes that record who asked.
pub struct Caller(pub Arc<Principal>);

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Caller(principal_of(parts)?))
    }
}

/// A caller holding the configured admin role.
pub struct Admin;

impl<S: Send + Sync> FromRequestParts<S> for Admin {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let p = principal_of(parts)?;
        if p.admin {
            Ok(Admin)
        } else {
            // No subject: an administrative refusal is worth a log line, and *who* was
            // refused is Keycloak's to record, not ours.
            tracing::warn!(tenant = %p.tenant.as_str(), "admin route refused");
            Err(AuthError::MissingRole)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_organization_that_could_escape_its_own_prefix_is_not_a_tenant() {
        assert_eq!(tenant_from("acme").map(|t| t.0), Some("acme".to_string()));
        assert_eq!(tenant_from("  acme-1_2 ").map(|t| t.0), Some("acme-1_2".to_string()));
        // Each of these reaches a blob key as `originals/<tenant>/…` or a hash key as a
        // `|`-joined part, so each is a way out of one organization's own namespace.
        for bad in ["", "   ", "../other", "a/b", "a|b", "a b", "a.b", "a:b", "ümlaut", "a\0b"] {
            assert!(tenant_from(bad).is_none(), "{bad:?} was accepted as an organization");
        }
        assert!(tenant_from(&"a".repeat(MAX_TENANT_LEN)).is_some());
        assert!(tenant_from(&"a".repeat(MAX_TENANT_LEN + 1)).is_none(), "an unbounded organization id");
    }

    #[test]
    fn claims_are_read_by_dotted_path_and_in_either_shape() {
        let claims = json!({
            "sub": "u1",
            "koki": { "org": "acme", "spaces": ["red", "blue"] },
            "single": ["only"],
            "spaced": "one two",
            "empty": "   ",
        });
        assert_eq!(claim_str(&claims, "koki.org").as_deref(), Some("acme"));
        assert_eq!(claim_str(&claims, "single").as_deref(), Some("only"), "a single-element array is one value");
        assert_eq!(claim_str(&claims, "empty"), None, "whitespace is not a value");
        assert_eq!(claim_str(&claims, "koki.missing"), None);
        assert_eq!(claim_str(&claims, "koki"), None, "an object is not a string");
        assert_eq!(claim_strings(&claims, "koki.spaces"), vec!["red", "blue"]);
        assert_eq!(claim_strings(&claims, "spaced"), vec!["one", "two"]);
        assert!(claim_strings(&claims, "nope").is_empty());
    }

    #[test]
    fn roles_come_from_the_realm_and_from_one_named_client() {
        let claims = json!({
            "realm_access": { "roles": ["offline_access", "ds-admin"] },
            "resource_access": {
                "ds-api": { "roles": ["ds-admin", "reader"] },
                "other-client": { "roles": ["ds-admin-elsewhere"] },
            },
        });
        assert_eq!(roles_from(&claims, Some("ds-api")), vec!["ds-admin", "offline_access", "reader"]);
        // Another client's roles are that client's business, not this API's.
        assert!(!roles_from(&claims, Some("ds-api")).iter().any(|r| r == "ds-admin-elsewhere"));
        assert_eq!(roles_from(&claims, None), vec!["ds-admin", "offline_access"]);
        assert!(roles_from(&json!({}), Some("ds-api")).is_empty());
    }
}
