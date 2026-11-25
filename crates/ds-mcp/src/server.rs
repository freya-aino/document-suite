//! The Model Context Protocol methods, over any transport.
//!
//! One `handle` function from an incoming message to an outgoing one, so the two
//! transports — stdio and Streamable HTTP — differ only in how bytes arrive. Which is the
//! point: a protocol bug reproduces in both, and a transport bug cannot look like a
//! protocol bug.

use serde_json::{json, Value};

use crate::jsonrpc::{Id, Incoming, Outgoing, RpcError};
use crate::suite::Suite;
use crate::tools::{self, Limits};

/// Protocol revisions this server implements, newest first.
///
/// Negotiation is what the specification asks for: a client sends the version it wants, a
/// server that supports it echoes it back, and a server that does not answers with one it
/// does support — the client then decides whether to continue. Echoing an *unknown*
/// version instead would be the dangerous reading: it would claim support for whatever a
/// future client asked for.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// What this server advertises when the client asks for something it does not know.
pub const PREFERRED_VERSION: &str = SUPPORTED_VERSIONS[0];

pub const SERVER_NAME: &str = "ds-mcp";

/// Shown to a model once, at the start of a session: the things it would otherwise have
/// to infer from eleven tool descriptions.
const INSTRUCTIONS: &str = "\
These tools read and submit documents in a visual-document-processing suite.

Two things shape everything here. **Processing is asynchronous**: upload_document returns a \
job id, and the text is readable only once wait_for_job reports done or partial. **Access \
is per person**: a document is private to whoever uploaded it until it is shared, so \
list_documents shows this caller's documents rather than the organization's whole corpus, \
and a refusal usually means asking the owner rather than retrying.

A useful order of operations: list_documents or upload_document, then wait_for_job, then \
get_document to see what was found, then get_document_text to read it. Everything is \
scoped to the caller's organization, which comes from their own credentials — there is no \
tenant argument anywhere, and no way to ask about another organization.";

/// Everything a tool call needs.
pub struct Server {
    pub suite: Suite,
    pub limits: Limits,
}

impl Server {
    /// Answers one message. `None` for a notification, which gets no reply at all.
    ///
    /// `token` is the bearer token this particular request carried, which only the HTTP
    /// transport has; stdio passes `None` and the token comes from the process's own
    /// environment (decision D16).
    pub async fn handle(&self, msg: Incoming, token: Option<&str>) -> Option<Outgoing> {
        let id = msg.id.clone();
        if msg.is_notification() {
            // `notifications/initialized` is the only one that matters, and nothing has to
            // happen on it: this server keeps no per-session state. Others are logged at
            // debug and ignored, which is what the specification asks for.
            tracing::debug!(method = %msg.method, "notification");
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        let result = self.dispatch(&msg.method, &msg.params, token).await;
        Some(match result {
            Ok(value) => Outgoing::result(id, value),
            Err(e) => {
                tracing::debug!(method = %msg.method, code = e.code.as_i32(), error = %e.message, "request refused");
                metrics::counter!("mcp_requests_total", "method" => msg.method.clone(), "outcome" => "error").increment(1);
                Outgoing::error(id, &e)
            }
        })
    }

    async fn dispatch(&self, method: &str, params: &Value, token: Option<&str>) -> Result<Value, RpcError> {
        let started = std::time::Instant::now();
        let out = match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools::list()),
            "tools/call" => self.call_tool(params, token).await,
            // Declared in no capability, so a conforming client will not ask — and one
            // that does gets the answer the specification prescribes rather than silence.
            "resources/list" | "resources/read" | "resources/templates/list" | "prompts/list" | "prompts/get" | "completion/complete" => {
                Err(RpcError::method_not_found(method)
                    .with_data(json!({ "hint": "this server advertises tools only; see the capabilities in the initialize result" })))
            }
            other => Err(RpcError::method_not_found(other)),
        };
        if out.is_ok() {
            metrics::counter!("mcp_requests_total", "method" => method.to_string(), "outcome" => "ok").increment(1);
            metrics::histogram!("mcp_request_duration_seconds", "method" => method.to_string()).record(started.elapsed().as_secs_f64());
        }
        out
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params["protocolVersion"].as_str().unwrap_or(PREFERRED_VERSION);
        let version = if SUPPORTED_VERSIONS.contains(&asked) { asked } else { PREFERRED_VERSION };
        let client = params["clientInfo"]["name"].as_str().unwrap_or("unknown");
        tracing::info!(client, asked, served = version, "session initialised");
        json!({
            "protocolVersion": version,
            "capabilities": {
                // No `listChanged`: the catalogue is compiled in, so it cannot change
                // under a client and promising a notification would be a promise never
                // kept.
                "tools": { "listChanged": false },
            },
            "serverInfo": { "name": SERVER_NAME, "title": "Document Suite", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    async fn call_tool(&self, params: &Value, token: Option<&str>) -> Result<Value, RpcError> {
        let name = params["name"].as_str().ok_or_else(|| RpcError::invalid_params("tools/call needs a tool `name`"))?;
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let (outcome, is_error) = tools::call(&self.suite, &self.limits, name, &args, token).await?;
        metrics::counter!("mcp_tool_calls_total", "tool" => name.to_string(), "outcome" => if is_error { "error" } else { "ok" })
            .increment(1);
        Ok(outcome.to_json(is_error))
    }
}

/// The reply to a message that could not even be parsed into a request.
pub fn refuse(id: Option<Id>, e: &RpcError) -> Outgoing {
    match id {
        Some(id) => Outgoing::error(id, e),
        None => Outgoing::error_without_id(e),
    }
}

/// Whether a protocol version is one this server implements.
pub fn knows_version(v: &str) -> bool {
    SUPPORTED_VERSIONS.contains(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::Code;
    use crate::suite::TokenSource;

    fn server() -> Server {
        Server {
            // Unreachable on purpose: these tests are about the protocol, and a call that
            // tried to reach the suite would be testing the suite.
            suite: Suite::new("http://127.0.0.1:1", TokenSource::None, std::time::Duration::from_millis(50)).expect("client"),
            limits: Limits::default(),
        }
    }

    fn request(method: &str, params: Value) -> Incoming {
        serde_json::from_value(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).expect("a request")
    }

    #[tokio::test]
    async fn initialize_echoes_a_known_version_and_substitutes_an_unknown_one() {
        let s = server();
        for known in SUPPORTED_VERSIONS {
            let out = s.handle(request("initialize", json!({ "protocolVersion": known })), None).await.expect("a reply");
            let result = out.result.expect("a result");
            assert_eq!(result["protocolVersion"], known, "a version this server knows must be echoed");
        }
        // A version from the future: answer with ours and let the client decide, rather
        // than claiming to speak something unknown.
        let out = s.handle(request("initialize", json!({ "protocolVersion": "2099-01-01" })), None).await.expect("a reply");
        let result = out.result.expect("a result");
        assert_eq!(result["protocolVersion"], PREFERRED_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        // No capability is advertised that this server does not implement.
        let caps = result["capabilities"].as_object().expect("capabilities");
        assert_eq!(caps.keys().collect::<Vec<_>>(), vec!["tools"]);
        assert!(result["instructions"].as_str().expect("instructions").contains("asynchronous"));
    }

    #[tokio::test]
    async fn a_notification_gets_no_reply() {
        let s = server();
        let note: Incoming = serde_json::from_value(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).expect("note");
        assert!(s.handle(note, None).await.is_none(), "answering a notification is a protocol violation");
    }

    #[tokio::test]
    async fn ping_is_an_empty_result_and_an_unknown_method_is_refused() {
        let s = server();
        let pong = s.handle(request("ping", json!({})), None).await.expect("a reply").result.expect("a result");
        assert_eq!(pong, json!({}));

        let out = s.handle(request("sorcery/invoke", json!({})), None).await.expect("a reply");
        let err = out.error.expect("an error");
        assert_eq!(err["code"], Code::MethodNotFound.as_i32());

        // A method from a capability this server does not advertise is refused with a
        // hint rather than an internal error.
        let out = s.handle(request("resources/list", json!({})), None).await.expect("a reply");
        let err = out.error.expect("an error");
        assert_eq!(err["code"], Code::MethodNotFound.as_i32());
        assert!(err["data"]["hint"].as_str().expect("a hint").contains("tools only"));
    }

    #[tokio::test]
    async fn tools_list_is_the_catalogue_and_needs_no_token() {
        let s = server();
        let out = s.handle(request("tools/list", json!({})), None).await.expect("a reply").result.expect("a result");
        assert_eq!(out["tools"].as_array().expect("tools").len(), tools::catalogue().len());
    }

    #[tokio::test]
    async fn an_unknown_tool_and_bad_arguments_are_protocol_errors() {
        let s = server();
        // The specification's own example for an unknown tool is -32602.
        let out = s.handle(request("tools/call", json!({ "name": "no_such_tool", "arguments": {} })), None).await.expect("a reply");
        assert_eq!(out.error.expect("an error")["code"], Code::InvalidParams.as_i32());

        // A missing required argument never reaches the suite.
        let out = s.handle(request("tools/call", json!({ "name": "get_document", "arguments": {} })), None).await.expect("a reply");
        let err = out.error.expect("an error");
        assert_eq!(err["code"], Code::InvalidParams.as_i32());
        assert!(err["message"].as_str().expect("message").contains("document_hk"), "{err}");

        // An invented argument is refused rather than ignored.
        let out = s
            .handle(request("tools/call", json!({ "name": "list_documents", "arguments": { "tenant": "someone-else" } })), None)
            .await
            .expect("a reply");
        let err = out.error.expect("an error");
        assert_eq!(err["code"], Code::InvalidParams.as_i32());
        assert!(err["message"].as_str().expect("message").contains("tenant"), "{err}");

        // And `tools/call` with no name at all.
        let out = s.handle(request("tools/call", json!({})), None).await.expect("a reply");
        assert_eq!(out.error.expect("an error")["code"], Code::InvalidParams.as_i32());
    }

    /// The distinction the specification draws, and the one most worth getting right: a
    /// tool that *ran* and failed reports it in a result the model can read, not in a
    /// JSON-RPC error the model never sees.
    #[tokio::test]
    async fn a_tool_that_cannot_reach_the_suite_reports_it_in_the_result() {
        let s = server();
        let out = s
            .handle(request("tools/call", json!({ "name": "get_document", "arguments": { "document_hk": "a".repeat(64) } })), None)
            .await
            .expect("a reply");
        assert!(out.error.is_none(), "a working tool with a failing call is not a protocol error");
        let result = out.result.expect("a result");
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().expect("text");
        // No token is configured here, so that is what it says — and it says what to do.
        assert!(text.contains("no access token"), "{text}");
        assert!(text.contains("DS_API_TOKEN"), "the model must be told how this is fixed: {text}");
    }

    #[test]
    fn the_supported_versions_are_ordered_newest_first() {
        let mut sorted = SUPPORTED_VERSIONS;
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sorted, SUPPORTED_VERSIONS, "PREFERRED_VERSION is the first entry, so the order is load-bearing");
        assert!(knows_version(PREFERRED_VERSION) && !knows_version("2000-01-01"));
    }
}
