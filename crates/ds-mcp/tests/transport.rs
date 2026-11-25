//! The two transports, each exercised the way a client uses it.
//!
//! The HTTP side runs the real router on a real socket. The stdio side runs the **built
//! binary** over pipes, which is the only way to test the thing that matters most there:
//! that nothing but protocol reaches stdout. A unit test cannot catch a stray `println!`
//! in a dependency's startup path; a subprocess can.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::{json, Value};
use ds_mcp::server::Server;
use ds_mcp::suite::{Suite, TokenSource};
use ds_mcp::tools::Limits;

async fn serve_http(allowed_origins: Vec<String>) -> String {
    let state = ds_mcp::http::HttpState {
        server: Arc::new(Server {
            // No suite behind it: every test here is about the transport, and a tool call
            // that fails to reach the API still returns a well-formed MCP result.
            suite: Suite::new("http://127.0.0.1:1", TokenSource::PerRequest, std::time::Duration::from_millis(50)).expect("client"),
            limits: Limits::default(),
        }),
        allowed_origins: Arc::new(allowed_origins),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, ds_mcp::http::router(state)).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn an_http_session_initialises_and_lists_its_tools() {
    let base = serve_http(vec![]).await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("{base}/mcp"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    // Stateless by design: no session id is assigned, so a client must not send one back.
    assert!(res.headers().get("mcp-session-id").is_none(), "this server assigns no session id");
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(body["id"], 1);

    // The notification that follows initialize gets no body, and the specification asks
    // for 202.
    let res = http
        .post(format!("{base}/mcp"))
        .header("mcp-protocol-version", "2025-06-18")
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 202);
    assert!(res.text().await.expect("text").is_empty());

    let body: Value = http
        .post(format!("{base}/mcp"))
        .header("mcp-protocol-version", "2025-06-18")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let tools = body["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), ds_mcp::tools::catalogue().len());
    assert!(tools.iter().any(|t| t["name"] == "get_document_text"));
}

#[tokio::test]
async fn the_authorization_header_is_what_a_tool_call_acts_as() {
    let base = serve_http(vec![]).await;
    let http = reqwest::Client::new();

    // No header: the tool says so rather than acting as anybody.
    let body: Value = http
        .post(format!("{base}/mcp"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_documents", "arguments": {} } }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("Authorization header"), "{text}");

    // With one, the call is attempted — and fails on the unreachable suite, not on the
    // token.
    let body: Value = http
        .post(format!("{base}/mcp"))
        .bearer_auth("alice-token")
        .json(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "list_documents", "arguments": {} } }))
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("json");
    let text = body["result"]["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("could not reach the suite"), "{text}");
    // And the token is never echoed back to the client.
    assert!(!serde_json::to_string(&body).expect("json").contains("alice-token"));
}

/// The attack the specification warns about for a locally bound server: a page on any
/// website can post to `127.0.0.1`, and a server that answers has handed that page the
/// tools — with the user's own token, in stdio-style deployments.
#[tokio::test]
async fn a_browser_origin_is_refused_unless_it_was_allowed() {
    let base = serve_http(vec!["http://localhost:3000".into()]).await;
    let http = reqwest::Client::new();
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });

    let res = http.post(format!("{base}/mcp")).header("origin", "https://evil.example").json(&msg).send().await.expect("send");
    assert_eq!(res.status(), 403);

    let res = http.post(format!("{base}/mcp")).header("origin", "http://localhost:3000").json(&msg).send().await.expect("send");
    assert_eq!(res.status(), 200);

    // A client that sends no Origin at all — every non-browser one — is unaffected.
    let res = http.post(format!("{base}/mcp")).json(&msg).send().await.expect("send");
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn the_endpoint_answers_the_methods_it_does_not_implement_the_way_mcp_allows() {
    let base = serve_http(vec![]).await;
    let http = reqwest::Client::new();

    // The SSE stream a server would push notifications down. This one has nothing to send.
    let res = http.get(format!("{base}/mcp")).send().await.expect("send");
    assert_eq!(res.status(), 405);
    assert_eq!(res.headers()[reqwest::header::ALLOW], "POST");
    assert!(res.text().await.expect("text").contains("no server-initiated messages"));

    // Session termination, where there are no sessions.
    let res = http.delete(format!("{base}/mcp")).send().await.expect("send");
    assert_eq!(res.status(), 405);
    assert!(res.text().await.expect("text").contains("stateless"));

    // A client that negotiated with a different server.
    let res = http
        .post(format!("{base}/mcp"))
        .header("mcp-protocol-version", "1999-12-31")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    assert!(res.text().await.expect("text").contains("2025-06-18"), "the refusal should say what this server does speak");

    assert_eq!(http.get(format!("{base}/healthz")).send().await.expect("send").status(), 200);
}

#[tokio::test]
async fn a_malformed_body_is_a_json_rpc_parse_error_with_a_null_id() {
    let base = serve_http(vec![]).await;
    let http = reqwest::Client::new();

    let res = http.post(format!("{base}/mcp")).body("{not json").send().await.expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["error"]["code"], -32700);
    assert_eq!(body["id"], Value::Null);

    // A batch, which MCP removed in 2025-06-18.
    let res = http
        .post(format!("{base}/mcp"))
        .json(&json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["error"]["code"], -32600);
    assert!(body["error"]["message"].as_str().expect("message").contains("one request per message"));
}

/// The stdio transport, through the built binary.
///
/// Three things are under test and only a subprocess can show them: that a session works
/// over pipes at all, that **stdout carries nothing but protocol** — the log line this
/// binary writes at startup has to be on stderr, or the client's first parse fails — and
/// that closing stdin ends the process.
#[test]
fn the_binary_speaks_the_protocol_over_stdio_and_keeps_its_logs_off_stdout() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds-mcp"))
        // Unreachable, deliberately: this test is the transport, not the API.
        .args(["--api", "http://127.0.0.1:1", "stdio"])
        .env("DS_API_TOKEN", "alice-token")
        // The startup log would land on stdout if the binary got this wrong, and `info`
        // guarantees there is one to land.
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary should start");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // Taking the pipe as an argument rather than capturing it, so the blank-line case
    // below can write to it too.
    let send = |stdin: &mut std::process::ChildStdin, msg: Value| {
        writeln!(stdin, "{msg}").expect("write");
        stdin.flush().expect("flush");
    };
    let recv = |stdout: &mut BufReader<std::process::ChildStdout>| -> Value {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read");
        assert!(!line.trim().is_empty(), "the server closed the stream instead of answering");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout must carry only JSON-RPC, got {line:?}: {e}"))
    };

    send(&mut stdin, json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }
    }));
    let reply = recv(&mut stdout);
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");

    // A notification: no reply, and the next request must still line up.
    send(&mut stdin, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    // A blank line, which some clients send as a keep-alive and which must not be
    // answered with a parse error.
    writeln!(stdin).expect("write");
    stdin.flush().expect("flush");

    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let reply = recv(&mut stdout);
    assert_eq!(reply["id"], 2, "a notification must not consume a reply slot");
    assert_eq!(reply["result"]["tools"].as_array().expect("tools").len(), ds_mcp::tools::catalogue().len());

    // An id arrives as a string and must come back as one.
    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": "abc", "method": "ping" }));
    let reply = recv(&mut stdout);
    assert_eq!(reply["id"], "abc");

    // A tool call: the token came from the environment, and the failure is the unreachable
    // suite — reported as a result the model can read.
    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "list_documents", "arguments": {} } }));
    let reply = recv(&mut stdout);
    assert_eq!(reply["result"]["isError"], true);
    assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("could not reach the suite"));

    // Closing stdin ends the session.
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "a closed stdin is a clean exit, got {:?}", out.status);
    assert!(out.stdout.is_empty(), "nothing should follow the last reply on stdout");

    // The logs went to stderr, where they belong, and the token is not among them.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mcp server on stdio"), "the startup log should be on stderr: {stderr}");
    assert!(!stderr.contains("alice-token"), "a token must never be logged: {stderr}");
}

/// Without a token the server still starts and still lists its tools, because a client
/// that launches it to look at the catalogue should not be met with an exit.
#[test]
fn the_binary_starts_without_a_token_and_says_what_is_missing() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds-mcp"))
        .args(["--api", "http://127.0.0.1:1", "stdio"])
        .env_remove("DS_API_TOKEN")
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    let reply: Value = serde_json::from_str(&line).expect("json");
    assert!(reply["result"]["tools"].as_array().expect("tools").len() > 5, "the catalogue needs no token");

    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no DS_API_TOKEN"), "the operator should be warned: {stderr}");
}
