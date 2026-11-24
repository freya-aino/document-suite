//! JSON-RPC 2.0, as much of it as the Model Context Protocol uses.
//!
//! Hand-written rather than taken from an SDK, for the reason `SPEC.md` records: this is
//! an envelope, three methods and a set of error codes, and the alternative is a
//! dependency whose own protocol revision we would then have to track. What it must get
//! right is narrow and testable, and `tests/protocol.rs` is what holds it to the wire
//! format.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A request id: a string or a number, and never null in a request.
///
/// Kept as a `Value` rather than normalised to a string, because a response **must** echo
/// the id it was sent — `1` and `"1"` are different ids to a client, and a server that
/// answers one with the other leaves the call unmatched forever.
pub type Id = Value;

/// One incoming message. A request has an `id`; a notification does not, and gets no
/// reply at all.
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Id>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Incoming {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// The JSON-RPC error codes this server uses.
///
/// `-32603 Internal error` is deliberately absent: a failure *inside* a tool is reported
/// as a result with `isError`, so the model sees it, and there is nothing left that an
/// internal error would describe. Adding it later is a line of code; having it unused
/// invites it to be used for the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Parse,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
}

impl Code {
    pub fn as_i32(self) -> i32 {
        match self {
            Code::Parse => -32700,
            Code::InvalidRequest => -32600,
            Code::MethodNotFound => -32601,
            Code::InvalidParams => -32602,
        }
    }
}

/// A protocol-level failure: the message could not be understood or dispatched.
///
/// Deliberately *not* how a tool reports that its work failed. A tool that could not read
/// a document returns a **result** with `isError: true`, because the model has to see it
/// and may be able to act on it (ask for access, wait, correct an argument); a JSON-RPC
/// error is for the client library and the model never sees its text. Confusing the two
/// is the most common way an MCP server becomes useless to the thing calling it, and
/// `tests/protocol.rs` pins which failures take which path.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: Code,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidParams, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(Code::MethodNotFound, format!("unknown method {method:?}"))
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// One outgoing message, ready to serialise.
#[derive(Debug, Clone, Serialize)]
pub struct Outgoing {
    pub jsonrpc: &'static str,
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl Outgoing {
    pub fn result(id: Id, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    pub fn error(id: Id, e: &RpcError) -> Self {
        let mut error = json!({ "code": e.code.as_i32(), "message": e.message });
        if let Some(data) = &e.data {
            error["data"] = data.clone();
        }
        Self { jsonrpc: "2.0", id, result: None, error: Some(error) }
    }

    /// An error for a message whose id could not be read — a parse failure, or a request
    /// with no id at all. JSON-RPC says to answer with a null id, which is the one place
    /// null is correct.
    pub fn error_without_id(e: &RpcError) -> Self {
        Self::error(Value::Null, e)
    }
}

/// Parses one message, distinguishing "not JSON" from "not a JSON-RPC request".
///
/// The `jsonrpc` field is checked but a missing one is tolerated: some clients omit it,
/// and refusing them buys nothing — the version is `"2.0"` or the message is malformed in
/// a way the shape check below already catches. A *wrong* version is refused, because
/// that is a client speaking something else.
pub fn parse(line: &str) -> Result<Incoming, RpcError> {
    let value: Value = serde_json::from_str(line).map_err(|e| RpcError::new(Code::Parse, format!("not JSON: {e}")))?;
    if value.is_array() {
        // Batching was removed from MCP in revision 2025-06-18. Refused rather than
        // half-supported: a client that batches and gets one of its requests answered has
        // a harder problem than one that is told plainly.
        return Err(RpcError::new(Code::InvalidRequest, "JSON-RPC batches are not supported (removed in MCP 2025-06-18); send one request per message"));
    }
    // The version is checked here, off the raw value, rather than kept on `Incoming`:
    // nothing downstream has a use for it, and a field nobody reads is a field that drifts.
    if let Some(v) = value.get("jsonrpc").and_then(Value::as_str) {
        if v != "2.0" {
            return Err(RpcError::new(Code::InvalidRequest, format!("unsupported jsonrpc version {v:?}")));
        }
    }
    serde_json::from_value(value).map_err(|e| RpcError::new(Code::InvalidRequest, format!("not a JSON-RPC request: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_and_a_notification_are_told_apart() {
        let req = parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).expect("a request");
        assert!(!req.is_notification());
        assert_eq!(req.id, Some(json!(1)));
        let note = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).expect("a notification");
        assert!(note.is_notification());
    }

    #[test]
    fn an_id_is_echoed_in_the_shape_it_arrived_in() {
        // `1` and `"1"` are different ids to a client, so normalising them would leave a
        // call unmatched forever.
        for raw in ["1", "\"1\"", "\"abc\""] {
            let req = parse(&format!(r#"{{"jsonrpc":"2.0","id":{raw},"method":"ping"}}"#)).expect("a request");
            let id = req.id.clone().expect("an id");
            let out = serde_json::to_value(Outgoing::result(id.clone(), json!({}))).expect("serialise");
            assert_eq!(out["id"], id);
            assert_eq!(out["jsonrpc"], "2.0");
            assert!(out.get("error").is_none(), "a result must not carry an error field");
        }
    }

    #[test]
    fn the_error_codes_are_the_standard_ones() {
        assert_eq!(Code::Parse.as_i32(), -32700);
        assert_eq!(Code::InvalidRequest.as_i32(), -32600);
        assert_eq!(Code::MethodNotFound.as_i32(), -32601);
        assert_eq!(Code::InvalidParams.as_i32(), -32602);
    }

    #[test]
    fn malformed_input_is_a_parse_error_and_a_wrong_shape_is_not() {
        assert_eq!(parse("{not json").expect_err("parse").code, Code::Parse);
        assert_eq!(parse(r#"{"jsonrpc":"2.0","id":1}"#).expect_err("no method").code, Code::InvalidRequest);
        assert_eq!(parse(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).expect_err("version").code, Code::InvalidRequest);
        // A batch is refused with a message that says what to do instead.
        let err = parse(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).expect_err("batch");
        assert_eq!(err.code, Code::InvalidRequest);
        assert!(err.message.contains("one request per message"), "{}", err.message);
        // A missing `jsonrpc` is tolerated: some clients omit it and refusing them buys
        // nothing.
        assert!(parse(r#"{"id":1,"method":"ping"}"#).is_ok());
    }

    #[test]
    fn an_unidentifiable_message_is_answered_with_a_null_id() {
        let out = serde_json::to_value(Outgoing::error_without_id(&RpcError::new(Code::Parse, "not JSON"))).expect("serialise");
        assert_eq!(out["id"], Value::Null, "JSON-RPC's one correct use of a null id");
        assert_eq!(out["error"]["code"], -32700);
        assert!(out.get("result").is_none(), "an error must not carry a result field");
    }
}
