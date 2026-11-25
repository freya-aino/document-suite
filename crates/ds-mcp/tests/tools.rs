//! The tools, against a suite served on a real socket.
//!
//! The fake here answers the shapes `ds-api` answers and records what it was asked, which
//! is what makes the two properties that matter testable: **the caller's own token reaches
//! the API on every call**, and **no tool argument can widen what the caller may see**.
//! `tests/live.rs` runs the same tools against the real router; this file is what runs
//! without Postgres.

use std::sync::{Arc, Mutex};

use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};

use ds_mcp::server::Server;
use ds_mcp::suite::{Suite, TokenSource};
use ds_mcp::tools::Limits;

/// What the fake suite was asked.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    /// The bearer token, as it arrived. The point of the whole file.
    token: Option<String>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Counters the fake needs between calls: how many polls a job has had, so
/// `wait_for_job` has something to wait for.
#[derive(Default)]
struct Counters {
    job_polls: u32,
}

/// A page result in the shape `ds-api` serves, with blocks deliberately out of reading
/// order so the tool has to sort them.
fn page(page_no: u32) -> Value {
    json!({
        "schema_version": 1,
        "document_hk": "a".repeat(64),
        "page_hk": "b".repeat(64),
        "page_no": page_no,
        "run_id": "3f2e1d00-0000-4000-8000-000000000000",
        "render_width_px": 1700,
        "render_height_px": 2200,
        "profile": { "doc_type": "invoice", "born_digital": true },
        "dag_ref": "born_digital_text@1",
        "blocks": [
            { "kind": "footer", "reading_order": 3, "text": "page 1 of 9", "content": { "type": "text", "text": "page 1 of 9" } },
            { "kind": "paragraph", "reading_order": 2, "text": format!("body of page {page_no}"), "content": { "type": "text", "text": format!("body of page {page_no}") } },
            { "kind": "heading", "reading_order": 1, "text": format!("Heading {page_no}"), "content": { "type": "text", "text": format!("Heading {page_no}") } },
            { "kind": "table", "reading_order": 4, "text": "1 2", "content": { "type": "table", "html": "<table><tr><td>1</td></tr></table>" } },
        ],
    })
}

/// A suite that answers the happy path, plus the refusals the tools have to handle.
///
/// One dispatcher rather than a route per endpoint: these tests are about what the MCP
/// server *sends*, so the path it sent is the thing worth matching on, and a `Router` of
/// closures would hide that behind extractors.
async fn fake_suite(log: Log) -> String {
    let counters = Arc::new(Mutex::new(Counters::default()));
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
        let log = log.clone();
        let counters = counters.clone();
        async move {
            let token = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string);
            let path = uri.to_string();
            log.lock().expect("log").push(Seen { method: method.to_string(), path: path.clone(), token });
            answer(&method, &path, &counters)
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn answer(method: &Method, path: &str, counters: &Mutex<Counters>) -> Response {
    let route = path.split('?').next().unwrap_or(path).to_string();
    let segments: Vec<&str> = route.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segments.as_slice()) {
        ("GET", ["v1", "documents"]) => Json(json!({
            "documents": [
                { "document_hk": "a".repeat(64), "visibility": "private", "owner_subject": "alice", "created_at": "2026-11-01T00:00:00Z" },
                { "document_hk": "c".repeat(64), "visibility": "organization", "owner_subject": null, "created_at": "2026-10-01T00:00:00Z" },
            ],
            "page": 0, "page_size": 50
        }))
        .into_response(),
        ("POST", ["v1", "documents"]) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "job_id": "11111111-0000-4000-8000-000000000000",
                "document_hk": "a".repeat(64),
                "run_id": "3f2e1d00-0000-4000-8000-000000000000",
                "status_url": "/v1/jobs/11111111-0000-4000-8000-000000000000"
            })),
        )
            .into_response(),
        // One document nobody may read, so the refusal path is exercised with the suite's
        // own wording.
        ("GET", ["v1", "documents", hk]) if hk.starts_with("ff") => (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "not readable by this caller; ask the document's owner to share it" })),
        )
            .into_response(),
        ("GET", ["v1", "documents", hk]) => Json(json!({
            "document_hk": hk,
            "run_id": "3f2e1d00-0000-4000-8000-000000000000",
            "run_state": "done",
            "dag_list_version": 1,
            "page_count": 2,
            "pages": [
                { "page_no": 1, "profile": { "doc_type": "invoice", "born_digital": true }, "dag_ref": "born_digital_text@1" },
                { "page_no": 2, "profile": { "doc_type": "invoice", "born_digital": true }, "dag_ref": "born_digital_text@1" },
            ],
            "blocks_by_kind": { "paragraph": 7, "table": 1 },
            "entities": 4,
            "assertions": 2
        }))
        .into_response(),
        // The document has two pages; a third is a 404, as it would be on the real API.
        ("GET", ["v1", "documents", _, "pages", n]) => match n.parse::<u32>() {
            Ok(n) if n <= 2 => Json(page(n)).into_response(),
            _ => (StatusCode::NOT_FOUND, Json(json!({ "message": "page not found" }))).into_response(),
        },
        ("GET", ["v1", "documents", _, "access"]) => Json(json!({
            "visibility": "private",
            "owner": "alice",
            "legacy": false,
            "grants": [{ "kind": "subject", "id": "bob" }]
        }))
        .into_response(),
        ("PUT", ["v1", "documents", _, "access"]) => Json(json!({ "visibility": "workspace" })).into_response(),
        ("POST", ["v1", "documents", _, "grants"]) => (StatusCode::CREATED, Json(json!({ "kind": "subject", "id": "bob" }))).into_response(),
        // 204, so the tool must cope with an empty body.
        ("DELETE", ["v1", "documents", _, "grants", _, _]) => StatusCode::NO_CONTENT.into_response(),
        ("GET", ["v1", "jobs", _]) => {
            let mut c = counters.lock().expect("counters");
            c.job_polls += 1;
            // Running, then running, then done: `wait_for_job` has to poll more than once
            // and stop as soon as the job is terminal.
            let state = if c.job_polls >= 3 { "done" } else { "pages_processing" };
            Json(json!({
                "job_id": "11111111-0000-4000-8000-000000000000",
                "state": state,
                "document_hk": "a".repeat(64),
                "run_id": "3f2e1d00-0000-4000-8000-000000000000",
                "pages_total": 2,
                "pages_done": if state == "done" { 2 } else { 1 },
                "pages_failed": 0,
                "created_at": "2026-11-01T00:00:00Z",
                "updated_at": "2026-11-01T00:01:00Z"
            }))
            .into_response()
        }
        ("POST", ["v1", "batches"]) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "batch_id": "22222222-0000-4000-8000-000000000000",
                "job_ids": ["11111111-0000-4000-8000-000000000000"],
                "failed_job_ids": [],
                "status_url": "/v1/batches/22222222-0000-4000-8000-000000000000"
            })),
        )
            .into_response(),
        ("GET", ["v1", "batches", _]) => Json(json!({
            "batch_id": "22222222-0000-4000-8000-000000000000",
            "created_at": "2026-11-01T00:00:00Z",
            "documents": 1,
            "total_jobs": 1,
            "counts": { "done": 1 },
            "page": 0,
            "page_size": 50,
            "jobs": []
        }))
        .into_response(),
        // Rate limiting, with the header the error is supposed to repeat back.
        ("GET", ["v1", "throttled"]) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "30")],
            "too many requests for this organization",
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, format!("the fake suite has no {method} {route}")).into_response(),
    }
}

fn server(base: &str, token: TokenSource) -> Server {
    Server {
        suite: Suite::new(base, token, std::time::Duration::from_secs(5)).expect("client"),
        limits: Limits { poll_interval: std::time::Duration::from_millis(20), ..Limits::default() },
    }
}

async fn call(server: &Server, name: &str, args: Value, token: Option<&str>) -> Value {
    let msg = serde_json::from_value(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    }))
    .expect("a request");
    let out = server.handle(msg, token).await.expect("a reply");
    assert!(out.error.is_none(), "{name} should not be a protocol error: {:?}", out.error);
    out.result.expect("a result")
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().expect("text").to_string()
}

const HK: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The property the whole design rests on: whoever calls the tool is who the API sees.
#[tokio::test]
async fn every_call_carries_the_callers_own_token() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::PerRequest);

    call(&s, "list_documents", json!({}), Some("alice-token")).await;
    call(&s, "get_document", json!({ "document_hk": HK }), Some("bob-token")).await;

    let seen = log.lock().expect("log").clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].token.as_deref(), Some("alice-token"));
    assert_eq!(seen[1].token.as_deref(), Some("bob-token"), "the second caller must not inherit the first's identity");

    // And a fixed token (the stdio case) is used when the request carries none.
    let s = server(&base, TokenSource::Fixed("env-token".into()));
    call(&s, "list_documents", json!({}), None).await;
    assert_eq!(log.lock().expect("log").last().expect("a call").token.as_deref(), Some("env-token"));
}

/// No tool takes a tenant, an organization or a subject-to-act-as, and none can be smuggled
/// in: the organization comes from the token and nowhere else (D11, D14, D16).
#[tokio::test]
async fn nothing_in_a_tool_argument_can_widen_what_the_caller_sees() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    // The schemas refuse the arguments outright...
    for (tool, args) in [
        ("list_documents", json!({ "tenant": "other-org" })),
        ("list_documents", json!({ "organization": "other-org" })),
        ("get_document", json!({ "document_hk": HK, "as_subject": "bob" })),
        ("get_document", json!({ "document_hk": HK, "owner_subject": "bob" })),
    ] {
        let msg = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        }))
        .expect("a request");
        let out = s.handle(msg, None).await.expect("a reply");
        assert!(out.error.is_some(), "{tool} accepted an argument that could widen access: {args}");
    }
    // ...and no request reached the suite at all.
    assert!(log.lock().expect("log").is_empty(), "a refused argument must not reach the API");

    // Nor does any tool's schema name one.
    let listed = ds_mcp::tools::list();
    for tool in listed["tools"].as_array().expect("tools") {
        let props = tool["inputSchema"]["properties"].as_object().expect("properties");
        for forbidden in ["tenant", "tenant_id", "organization", "org", "subject", "as_subject", "owner", "owner_subject", "token"] {
            assert!(
                !props.contains_key(forbidden),
                "{} takes a {forbidden} argument; identity must come from the token alone",
                tool["name"]
            );
        }
    }
}

#[tokio::test]
async fn reading_a_document_returns_its_text_in_reading_order_without_the_furniture() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    let result = call(&s, "get_document_text", json!({ "document_hk": HK, "pages": 2 }), None).await;
    let text = text_of(&result);
    assert_eq!(result["isError"], false);
    // Reading order, not the order the blocks arrived in.
    let heading = text.find("Heading 1").expect("the heading");
    let body = text.find("body of page 1").expect("the body");
    assert!(heading < body, "blocks must come out in reading order:\n{text}");
    // A table is HTML, because that is what it was stored as.
    assert!(text.contains("<table>"), "{text}");
    // Running footers are left out by default: they repeat and say nothing.
    assert!(!text.contains("page 1 of 9"), "furniture should be omitted:\n{text}");
    // Both pages, and the structured copy agrees with the text.
    assert!(text.contains("page 2"), "{text}");
    assert_eq!(result["structuredContent"]["last_page"], 2);
    assert!(result["structuredContent"]["blocks"].as_array().expect("blocks").len() >= 6);

    // Asking for furniture gets it.
    let with = call(&s, "get_document_text", json!({ "document_hk": HK, "pages": 1, "include_furniture": true }), None).await;
    assert!(text_of(&with).contains("page 1 of 9"));
}

/// A range that runs off the end of the document is an answer, not a failure.
#[tokio::test]
async fn a_page_range_past_the_end_returns_what_exists() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    let result = call(&s, "get_document_text", json!({ "document_hk": HK, "first_page": 1, "pages": 5 }), None).await;
    assert_eq!(result["isError"], false, "a short document is not an error");
    assert_eq!(result["structuredContent"]["last_page"], 2);

    // But a first page that does not exist at all says so, rather than returning nothing
    // without explanation.
    let result = call(&s, "get_document_text", json!({ "document_hk": HK, "first_page": 9, "pages": 1 }), None).await;
    assert_eq!(result["isError"], true);
    assert!(text_of(&result).contains("not found"), "{}", text_of(&result));
}

/// The cap is the server's, so a model cannot ask for a context window's worth of text.
#[tokio::test]
async fn a_long_answer_is_truncated_and_says_so() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let mut s = server(&base, TokenSource::Fixed("alice-token".into()));
    s.limits.max_text_chars = 1_000;
    s.limits.max_pages = 2;

    let result = call(&s, "get_document_text", json!({ "document_hk": HK, "pages": 2 }), None).await;
    let text = text_of(&result);
    assert!(text.chars().count() < 1_400, "the cap must hold: {} characters", text.chars().count());

    // And a request for more pages than the server allows is clamped rather than refused:
    // asking for 500 pages is a reasonable thing for a model to do once.
    let before = log.lock().expect("log").len();
    call(&s, "get_document_text", json!({ "document_hk": HK, "pages": 500 }), None).await;
    let fetched = log.lock().expect("log").len() - before;
    assert!(fetched <= 2, "the page cap must bound the number of API calls, got {fetched}");
}

#[tokio::test]
async fn waiting_for_a_job_polls_until_it_is_terminal_and_then_stops() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    let result = call(&s, "wait_for_job", json!({ "job_id": "11111111-0000-4000-8000-000000000000" }), None).await;
    let text = text_of(&result);
    assert_eq!(result["isError"], false);
    assert!(text.contains("done"), "{text}");
    assert!(text.contains("get_document_text"), "a finished job should say what to do next: {text}");
    // The fake turns done on the third poll; stopping there rather than polling on is the
    // behaviour under test.
    let polls = log.lock().expect("log").iter().filter(|s| s.path.contains("/v1/jobs/")).count();
    assert_eq!(polls, 3, "it must stop as soon as the job is terminal");
}

/// A wait that runs out is not a failure: the job keeps going.
#[tokio::test]
async fn a_wait_that_times_out_says_the_job_is_still_running() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let mut s = server(&base, TokenSource::Fixed("alice-token".into()));
    // The fake needs three polls; one is all this gets.
    s.limits.max_wait = std::time::Duration::from_millis(30);
    s.limits.poll_interval = std::time::Duration::from_millis(25);

    let result = call(&s, "wait_for_job", json!({ "job_id": "11111111-0000-4000-8000-000000000000", "timeout_seconds": 600 }), None).await;
    let text = text_of(&result);
    assert_eq!(result["isError"], false, "a timed-out wait is not a tool failure");
    assert!(text.contains("Still running"), "{text}");
    assert!(text.contains("wait_for_job can be called again"), "{text}");
}

#[tokio::test]
async fn submitting_a_document_returns_a_job_and_tells_the_model_what_to_do_with_it() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    let result = call(
        &s,
        "upload_document",
        json!({ "source_uri": "originals/acme/invoice.pdf", "language": "en", "priority": "interactive", "doc_type": "invoice" }),
        None,
    )
    .await;
    assert_eq!(result["isError"], false);
    let text = text_of(&result);
    assert!(text.contains("11111111-0000-4000-8000-000000000000"), "{text}");
    assert!(text.contains("wait_for_job"), "{text}");

    // `skip_if_processed` is a query parameter, not a body field.
    call(&s, "upload_document", json!({ "source_uri": "k", "language": "en", "skip_if_processed": true }), None).await;
    let last = log.lock().expect("log").last().expect("a call").clone();
    assert!(last.path.contains("skip_if_processed=true"), "{}", last.path);

    let result = call(
        &s,
        "upload_batch",
        json!({ "documents": [{ "source_uri": "originals/acme/a.pdf" }], "language": "de", "profile": "fast" }),
        None,
    )
    .await;
    assert!(text_of(&result).contains("22222222-0000-4000-8000-000000000000"));
    assert!(text_of(&result).contains("get_batch"));
}

#[tokio::test]
async fn sharing_and_unsharing_use_the_routes_they_claim_to() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    let result = call(&s, "get_document_access", json!({ "document_hk": HK }), None).await;
    let text = text_of(&result);
    assert!(text.contains("private") && text.contains("alice") && text.contains("subject bob"), "{text}");

    call(&s, "share_document", json!({ "document_hk": HK, "visibility": "workspace" }), None).await;
    assert_eq!(log.lock().expect("log").last().expect("a call").method, "PUT");

    call(&s, "grant_access", json!({ "document_hk": HK, "kind": "subject", "id": "bob" }), None).await;
    assert_eq!(log.lock().expect("log").last().expect("a call").method, "POST");

    // A 204 with no body must not look like a malformed answer.
    let result = call(&s, "revoke_access", json!({ "document_hk": HK, "kind": "subject", "id": "bob@example.com" }), None).await;
    assert_eq!(result["isError"], false, "{}", text_of(&result));
    let last = log.lock().expect("log").last().expect("a call").clone();
    assert_eq!(last.method, "DELETE");
    // The id is percent-encoded into its own path segment.
    assert!(last.path.ends_with("/grants/subject/bob%40example.com"), "{}", last.path);
}

/// What a model is told when it may not read something — the most common refusal, and the
/// one where the right advice matters most.
#[tokio::test]
async fn a_refusal_comes_back_as_a_readable_result_with_the_suites_own_reason() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let s = server(&base, TokenSource::Fixed("alice-token".into()));

    let forbidden = "ff".to_string() + &"a".repeat(62);
    let result = call(&s, "get_document", json!({ "document_hk": forbidden }), None).await;
    assert_eq!(result["isError"], true);
    assert!(result["error"].is_null(), "a refusal is a result, not a JSON-RPC error");
    let text = text_of(&result);
    // The suite's own sentence, which already names the remedy...
    assert!(text.contains("ask the document's owner to share it"), "{text}");
    // ...and this server's, which names the tool that does it.
    assert!(text.contains("share_document") || text.contains("grant_access"), "{text}");
}

#[tokio::test]
async fn a_rate_limit_tells_the_model_how_long_to_wait() {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let base = fake_suite(log.clone()).await;
    let suite = Suite::new(&base, TokenSource::Fixed("alice-token".into()), std::time::Duration::from_secs(5)).expect("client");
    // Straight at the client, because no tool maps to a route that throttles on demand.
    let err = suite.call_raw(reqwest::Method::GET, "/v1/throttled", None, None).await.expect_err("429");
    assert!(err.to_string().contains("30s"), "{err}");
    assert!(err.advice().contains("per organization"), "{}", err.advice());
}
