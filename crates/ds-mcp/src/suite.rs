//! The client this service is a client of: `ds-suite`'s HTTP API.
//!
//! **This service holds no credentials of its own** (decision D16). Every call carries the
//! bearer token of whoever is calling it, and when there is none the tool says so instead
//! of falling back to anything. That is not caution for its own sake: access inside an
//! organization is the person's (D14), and a service account here would collapse it to
//! "whatever the MCP service can see" — every model user reading every document the
//! service's own identity could reach, which is exactly the hole the access work closed.
//!
//! Where the token comes from depends on the transport, and both answers are the caller's:
//!
//! | Transport | Token |
//! |---|---|
//! | stdio | `DS_API_TOKEN` in the environment of the process the user started, or `--token-file` |
//! | HTTP | the `Authorization` header of the MCP request, forwarded unchanged |
//!
//! The HTTP case is also what MCP's own authorization model says: the server is a resource
//! server and the client presents a token for it. The suite is the thing that verifies it
//! — this service never inspects, parses or logs it.

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

/// What a tool call can go wrong with.
///
/// Every variant is something a model can be told and may be able to act on, which is why
/// these become `isError` results rather than JSON-RPC errors — see [`crate::jsonrpc`].
#[derive(Debug, thiserror::Error)]
pub enum SuiteError {
    #[error("no access token: {0}")]
    NoToken(String),
    #[error("the suite refused the token: {0}")]
    Unauthorized(String),
    #[error("not permitted: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// 429. `retry_after` is the suite's own `Retry-After`, in seconds, when it sent one.
    #[error("rate limited{}: {message}", match retry_after { Some(s) => format!(", retry after {s}s"), None => String::new() })]
    RateLimited { message: String, retry_after: Option<u64> },
    #[error("the suite rejected the request: {0}")]
    BadRequest(String),
    #[error("the suite is unavailable: {0}")]
    Unavailable(String),
    /// The field is `reason` rather than `source`, because `thiserror` reads a field of
    /// that name as a nested error and this one is a message.
    #[error("could not reach the suite at {base}: {reason}")]
    Transport { base: String, reason: String },
    #[error("the suite answered something this client cannot read: {0}")]
    Malformed(String),
}

impl SuiteError {
    /// A sentence telling the model what to do about it, appended to the error text.
    ///
    /// A tool error that only says what happened makes the model guess at the remedy, and
    /// the remedies here are specific: a 403 on a document is a sharing problem with a
    /// known fix, a 429 is a waiting problem with a known duration.
    pub fn advice(&self) -> &str {
        match self {
            SuiteError::NoToken(_) => "Ask the person running this server to set DS_API_TOKEN (stdio) or to send an Authorization header (HTTP). This server cannot obtain a token itself.",
            SuiteError::Unauthorized(_) => "The token is missing, expired or revoked. A fresh one is needed; this server cannot refresh it.",
            SuiteError::Forbidden(_) => "The caller is authenticated but not permitted. For a document, its owner can share it with `share_document` or `grant_access`.",
            SuiteError::NotFound(_) => "Check the identifier. A document hash key is 64 hex characters and a job id is a UUID.",
            SuiteError::RateLimited { .. } => "Wait for the stated interval and try again; the limit is per organization.",
            SuiteError::BadRequest(_) => "Correct the arguments and try again.",
            SuiteError::Unavailable(_) => "The suite is temporarily unavailable. Retrying after a few seconds is reasonable.",
            SuiteError::Transport { .. } => "The suite could not be reached at all. This is a deployment problem rather than something to retry hard.",
            SuiteError::Malformed(_) => "This is a defect in this server or a version mismatch with the suite; report it rather than retrying.",
        }
    }
}

/// How a call gets its bearer token.
#[derive(Debug, Clone)]
pub enum TokenSource {
    /// A token read once at startup, from `DS_API_TOKEN` or `--token-file`. The stdio
    /// case: the process is the user's, so its environment is the user's.
    Fixed(Arc<str>),
    /// Taken from each MCP request. The HTTP case.
    PerRequest,
    /// Nothing configured. Every call fails with a message saying how to fix it, which is
    /// a better failure than a server that starts and then silently acts as nobody.
    None,
}

/// The suite's API, as this service uses it.
pub struct Suite {
    http: reqwest::Client,
    base: String,
    token: TokenSource,
}

impl Suite {
    pub fn new(base: impl Into<String>, token: TokenSource, timeout: Duration) -> anyhow::Result<Self> {
        let base = base.into().trim_end_matches('/').to_string();
        // One client, so connections are pooled across tool calls. The timeout lives on
        // the client rather than on this struct: every call goes through it, so a second
        // copy of the number would only be a way to disagree with itself.
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { http, base, token })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The token for this call: the per-request one when the transport supplies it,
    /// otherwise the fixed one.
    fn bearer(&self, request_token: Option<&str>) -> Result<String, SuiteError> {
        if let Some(t) = request_token.map(str::trim).filter(|t| !t.is_empty()) {
            return Ok(t.to_string());
        }
        match &self.token {
            TokenSource::Fixed(t) => Ok(t.to_string()),
            TokenSource::PerRequest => Err(SuiteError::NoToken(
                "this server takes the caller's token from the request's Authorization header, and this request carried none".into(),
            )),
            TokenSource::None => Err(SuiteError::NoToken("no token is configured and none was sent with the request".into())),
        }
    }

    /// One API call. `body` is `None` for a GET.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        request_token: Option<&str>,
    ) -> Result<T, SuiteError> {
        let text = self.call_raw(method, path, body, request_token).await?;
        // An empty body is a valid answer for a 204 and for `()`; `null` parses as both.
        let text = if text.trim().is_empty() { "null".to_string() } else { text };
        serde_json::from_str(&text).map_err(|e| SuiteError::Malformed(format!("{e}: {}", truncate(&text, 400))))
    }

    /// The response body as text, with the status mapped to a [`SuiteError`].
    pub async fn call_raw(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        request_token: Option<&str>,
    ) -> Result<String, SuiteError> {
        let token = self.bearer(request_token)?;
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut req = self.http.request(method.clone(), &url).bearer_auth(token);
        if let Some(b) = &body {
            req = req.json(b);
        }
        let started = std::time::Instant::now();
        let res = req.send().await.map_err(|e| SuiteError::Transport { base: self.base.clone(), reason: e.to_string() })?;
        let status = res.status();
        let retry_after = res.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
        let text = res.text().await.unwrap_or_default();
        metrics::histogram!("mcp_api_call_duration_seconds", "path" => path.to_string(), "status" => status.as_u16().to_string())
            .record(started.elapsed().as_secs_f64());
        // The token is never logged, and neither is the body: a job spec carries the
        // caller's subject and a page result carries the document's text.
        tracing::debug!(%method, path, status = status.as_u16(), elapsed_ms = started.elapsed().as_millis() as u64, "suite call");

        if status.is_success() {
            return Ok(text);
        }
        // The suite's own message is passed through: its 403 already says "ask the
        // document's owner to share it", which is better advice than this service could
        // invent, and its 400s name the field.
        let message = api_message(&text);
        Err(match status.as_u16() {
            401 => SuiteError::Unauthorized(message),
            403 => SuiteError::Forbidden(message),
            404 => SuiteError::NotFound(message),
            429 => SuiteError::RateLimited { message, retry_after },
            400 | 409 | 413 | 422 => SuiteError::BadRequest(message),
            500..=599 => SuiteError::Unavailable(message),
            other => SuiteError::BadRequest(format!("HTTP {other}: {message}")),
        })
    }
}

/// The message out of an error body: the suite answers some routes with plain text and
/// others with JSON, so both are read rather than one guessed at.
fn api_message(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return "no message".into();
    }
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        for key in ["error_description", "message", "error"] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                return truncate(s, 400);
            }
        }
    }
    truncate(body, 400)
}

/// Shortens on a character boundary, marking that it did.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_token_wins_over_a_fixed_one_and_an_absent_one_is_explained() {
        let fixed = Suite::new("http://x", TokenSource::Fixed("from-env".into()), Duration::from_secs(1)).expect("client");
        assert_eq!(fixed.bearer(None).expect("the fixed token"), "from-env");
        assert_eq!(fixed.bearer(Some("from-request")).expect("the request's"), "from-request");
        // Whitespace is not a token: a header of `Bearer ` must not become one.
        assert_eq!(fixed.bearer(Some("   ")).expect("falls back"), "from-env");

        let per = Suite::new("http://x", TokenSource::PerRequest, Duration::from_secs(1)).expect("client");
        let err = per.bearer(None).expect_err("no token");
        assert!(matches!(err, SuiteError::NoToken(_)));
        assert!(err.advice().contains("Authorization header"), "the advice must say how to fix it: {}", err.advice());

        let none = Suite::new("http://x", TokenSource::None, Duration::from_secs(1)).expect("client");
        assert!(matches!(none.bearer(None), Err(SuiteError::NoToken(_))));
    }

    #[test]
    fn the_base_url_is_normalised_once() {
        let s = Suite::new("http://127.0.0.1:8080/", TokenSource::None, Duration::from_secs(1)).expect("client");
        assert_eq!(s.base(), "http://127.0.0.1:8080");
    }

    #[test]
    fn the_suites_own_message_is_what_the_model_is_told() {
        // Plain text, which is what most of the API's refusals are.
        assert_eq!(api_message("not readable by this caller; ask the document's owner to share it"), "not readable by this caller; ask the document's owner to share it");
        // JSON, which is what back-channel logout and a few others answer with.
        assert_eq!(api_message(&json!({"error":"invalid_request","error_description":"stale"}).to_string()), "stale");
        assert_eq!(api_message(&json!({"message":"job not found"}).to_string()), "job not found");
        assert_eq!(api_message(""), "no message");
        assert!(api_message(&"x".repeat(1000)).ends_with("[truncated]"));
    }

    #[test]
    fn every_error_carries_advice_that_names_the_remedy() {
        let cases: Vec<SuiteError> = vec![
            SuiteError::NoToken("x".into()),
            SuiteError::Unauthorized("x".into()),
            SuiteError::Forbidden("x".into()),
            SuiteError::NotFound("x".into()),
            SuiteError::RateLimited { message: "x".into(), retry_after: Some(30) },
            SuiteError::BadRequest("x".into()),
            SuiteError::Unavailable("x".into()),
            SuiteError::Transport { base: "http://x".into(), reason: "refused".into() },
            SuiteError::Malformed("x".into()),
        ];
        for e in cases {
            assert!(e.advice().len() > 20, "{e} has no useful advice");
            assert!(e.advice().ends_with('.'), "{e}: advice reads as a sentence");
        }
        // The retry interval reaches the message, because "wait" without a number is not
        // actionable.
        let limited = SuiteError::RateLimited { message: "too many".into(), retry_after: Some(30) };
        assert!(limited.to_string().contains("30s"), "{limited}");
    }

    #[test]
    fn truncation_is_marked_and_lands_on_a_character_boundary() {
        assert_eq!(truncate("short", 10), "short");
        let multi = "äöü".repeat(10);
        let cut = truncate(&multi, 5);
        assert!(cut.starts_with("äöüäö") && cut.ends_with("[truncated]"), "{cut}");
    }
}
