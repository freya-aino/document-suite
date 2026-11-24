//! The Streamable HTTP transport: one endpoint, `POST` a request, get a reply.
//!
//! **Stateless, deliberately.** The specification lets a server assign an `Mcp-Session-Id`
//! at initialize and require it afterwards; this one does not, because it keeps nothing
//! between calls — every request carries its own bearer token, which is the only state a
//! call needs (decision D16). A client that gets no session id must not send one, so the
//! protocol stays conformant, and the deployment consequence is worth having: copies need
//! no sticky sessions and no shared store, which is the same reasoning D13 applied to the
//! suite itself.
//!
//! What that costs: no server-initiated messages. `GET` on the endpoint — the SSE stream a
//! server would push notifications or sampling requests down — answers `405`, which the
//! specification explicitly allows for a server that offers none. Nothing in this
//! catalogue needs one: every tool is a question with an answer.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::jsonrpc::{self, Code, RpcError};
use crate::server::{self, Server};

#[derive(Clone)]
pub struct HttpState {
    pub server: Arc<Server>,
    /// Origins a browser-based client may use. Empty means "refuse every request that
    /// carries an `Origin`", which is the safe default for a server bound to localhost.
    pub allowed_origins: Arc<Vec<String>>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/mcp", post(message).get(no_stream).delete(no_session))
        // A liveness probe, because this is a service and something will want to know it
        // is up. Deliberately not on `/mcp`: that path is the protocol's.
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

/// One JSON-RPC message.
async fn message(State(s): State<HttpState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = guard_origin(&s, &headers) {
        return response;
    }
    if let Err(response) = guard_protocol_version(&headers) {
        return response;
    }

    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(_) => {
            let e = RpcError::new(Code::Parse, "the body is not UTF-8");
            return (StatusCode::BAD_REQUEST, Json(server::refuse(None, &e))).into_response();
        }
    };
    let msg = match jsonrpc::parse(text) {
        Ok(msg) => msg,
        Err(e) => {
            // 400 with a JSON-RPC error body: the HTTP status is for the transport and the
            // body is for the client's protocol layer, and a client reads both.
            return (StatusCode::BAD_REQUEST, Json(server::refuse(None, &e))).into_response();
        }
    };

    // The caller's own token, forwarded unchanged and never inspected here (D16).
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim().to_string());

    match s.server.handle(msg, token.as_deref()).await {
        // A notification has no reply, and the specification asks for 202 Accepted.
        None => StatusCode::ACCEPTED.into_response(),
        Some(reply) => (StatusCode::OK, Json(reply)).into_response(),
    }
}

/// `GET /mcp` is the server-to-client SSE stream. This server has nothing to send down it.
async fn no_stream() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "this server sends no server-initiated messages, so it offers no SSE stream (MCP allows a 405 here)",
    )
        .into_response()
}

/// `DELETE /mcp` ends a session. There are none to end.
async fn no_session() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "this server is stateless and assigns no Mcp-Session-Id, so there is no session to terminate",
    )
        .into_response()
}

/// Refuses a cross-origin request from a browser.
///
/// The specification calls this out for locally bound servers because of DNS rebinding: a
/// page on any website can POST to `127.0.0.1`, and a server that answers it has handed a
/// stranger's JavaScript the tools — with the user's own token, if one is configured in
/// the environment. A request with **no** `Origin` is allowed, because that is every
/// non-browser client; one *with* an Origin must be on the list.
fn guard_origin(s: &HttpState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    if s.allowed_origins.iter().any(|a| a == origin) {
        return Ok(());
    }
    tracing::warn!(origin, "refused a cross-origin MCP request");
    metrics::counter!("mcp_origin_refusals_total").increment(1);
    Err((
        StatusCode::FORBIDDEN,
        format!("origin {origin:?} is not allowed; pass --allowed-origin to permit it"),
    )
        .into_response())
}

/// A client that states a protocol version must state one this server implements.
///
/// Sent on every request after initialize in revision 2025-06-18. A missing header is
/// allowed — older clients do not send it — but a *wrong* one is a client that negotiated
/// with a different server, and continuing would mean guessing which dialect the next
/// message is in.
fn guard_protocol_version(headers: &HeaderMap) -> Result<(), Response> {
    let Some(v) = headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    if server::knows_version(v.trim()) {
        return Ok(());
    }
    Err((
        StatusCode::BAD_REQUEST,
        format!(
            "unsupported MCP-Protocol-Version {v:?}; this server implements {}",
            server::SUPPORTED_VERSIONS.join(", ")
        ),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(origins: Vec<String>) -> HttpState {
        HttpState {
            server: Arc::new(Server {
                suite: crate::suite::Suite::new("http://127.0.0.1:1", crate::suite::TokenSource::PerRequest, std::time::Duration::from_millis(50))
                    .expect("client"),
                limits: crate::tools::Limits::default(),
            }),
            allowed_origins: Arc::new(origins),
        }
    }

    fn with(header: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HeaderName::from_bytes(header.as_bytes()).expect("header"), value.parse().expect("value"));
        h
    }

    #[test]
    fn a_request_without_an_origin_is_allowed_and_a_foreign_one_is_not() {
        let s = state(vec!["http://localhost:3000".into()]);
        // Every non-browser client: no Origin at all.
        assert!(guard_origin(&s, &HeaderMap::new()).is_ok());
        assert!(guard_origin(&s, &with("origin", "http://localhost:3000")).is_ok());
        // The attack the specification warns about: a page on any site posting to a
        // locally bound server, which would otherwise act with the user's own token.
        let refused = guard_origin(&s, &with("origin", "https://evil.example")).expect_err("refused");
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        // With no allowlist, any stated origin is refused.
        assert!(guard_origin(&state(vec![]), &with("origin", "http://localhost:3000")).is_err());
    }

    #[test]
    fn a_stated_protocol_version_must_be_one_this_server_speaks() {
        assert!(guard_protocol_version(&HeaderMap::new()).is_ok(), "an older client sends no header");
        for v in server::SUPPORTED_VERSIONS {
            assert!(guard_protocol_version(&with("mcp-protocol-version", v)).is_ok());
        }
        let refused = guard_protocol_version(&with("mcp-protocol-version", "1999-12-31")).expect_err("refused");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    }
}
