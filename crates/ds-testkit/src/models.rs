//! A fake OpenAI-compatible model tier.
//!
//! **Dispatch is on the task, read from the prompt, not on the model id.** The id alone
//! cannot decide: one registry entry serves several tasks — `mineru-2.5` is declared for
//! `page_parse` *and* `layout` — so a parse request and a layout request arrive at the
//! same model with different jobs to do. Each of the suite's prompts opens with a
//! distinctive sentence, and that is what is matched here. An unrecognised prompt is an
//! error rather than a guess: a fake that silently answers the wrong shape sends the
//! caller into the Markdown fallback, which looks like a model problem for a long time.
//!
//! The answers are **derived from the request** wherever a real one would be. Extraction
//! quotes come out of the window it was shown, so `ground_output` keeps the claim;
//! verification echoes the ids it was given, so a verdict cannot verify an assertion
//! nobody emitted. Returning plausible constants instead would let the grounding and
//! verification paths be skipped entirely while every assertion still passed.
//!
//! What it is not: a model. It measures that the suite is wired, never how well a page
//! is parsed.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

/// Embedding width. Eight, not 1024: enough to exercise the dimension guard, the aliased
/// collection and the fusion, without carrying a kilobyte per block through a test.
pub const DIMS: usize = 8;

/// How large a request this fake accepts.
///
/// Axum's default is 2 MB, and that is **not enough for a page**: a 200-dpi A4 render is
/// around 1 MB of PNG, base64 in a JSON message is a third larger again, and a page that
/// has been through a conditioner is a re-encode that can be larger still. The chain
/// found this the hard way — every `conditioned-e2e` candidate on a scanned corpus failed
/// with `413 length limit exceeded`, which arrives as "model call failed" and reads like
/// a model problem.
///
/// A real VLM server accepts multi-megabyte images, so the default made the fake *stricter
/// than production* — the worst kind of difference for a test double to have.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// The routes a model tier serves, plus `/health` so compose and vLLM agree on what
/// "up" means.
pub fn router() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/embeddings", post(embeddings))
        .route("/health", get(|| async { "ok" }))
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES))
}

/// Serves on an ephemeral localhost port and returns its base URL, ready for a
/// registry's `endpoints`.
///
/// On a real socket rather than as a fake client, for the reason both of the tests this
/// replaces gave: a fake client skips the transport, the registry's health ejection and
/// the parser — the three things most likely to be wrong.
pub async fn spawn() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind a port for the fake model tier");
    let port = listener.local_addr().expect("local addr").port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router()).await.expect("serve the fake model tier");
    });
    (format!("http://127.0.0.1:{port}/v1"), handle)
}

/// The task a prompt is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    PageParse,
    Layout,
    PageClassify,
    DocumentClassify,
    RegionOcr,
    Table,
    Formula,
    FigureDescribe,
    ChartToTable,
    Extract,
    Verify,
}

/// Which task a prompt is asking for, by the opening sentence the suite's own prompt
/// files carry. Ordered so the more specific wording is tested first.
pub fn task_of(prompt: &str) -> Option<Task> {
    const MARKERS: &[(&str, Task)] = &[
        ("Parse this document page completely", Task::PageParse),
        ("You are a document layout analyser", Task::Layout),
        ("You are classifying a single document page image", Task::PageClassify),
        ("You are identifying what kind of document", Task::DocumentClassify),
        ("Transcribe all text in this image region", Task::RegionOcr),
        ("Convert this table image into HTML", Task::Table),
        ("Transcribe the mathematical formula", Task::Formula),
        ("Describe this figure from a document", Task::FigureDescribe),
        ("Convert this chart from a document into data", Task::ChartToTable),
        ("You extract structured knowledge", Task::Extract),
        ("You check whether assertions extracted", Task::Verify),
    ];
    MARKERS.iter().find(|(m, _)| prompt.contains(m)).map(|(_, t)| *t)
}

fn completion(content: String) -> Value {
    // `usage` is reported because `model_tokens_total` is incremented from it, and the
    // integration suite reads that counter to get tokens per document — the one number
    // `RunRecord::tokens` cannot supply, since no token count reaches the executor.
    let prompt_tokens = 64;
    let completion_tokens = content.split_whitespace().count().max(1);
    json!({
        "choices": [{ "message": { "content": content }, "finish_reason": "stop" }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    })
}

async fn chat(Json(req): Json<Value>) -> axum::response::Response {
    let prompt = prompt_text(&req);
    let Some(task) = task_of(&prompt) else {
        // Loud, and with the opening of the prompt in it, because the alternative is a
        // test that fails three layers away in a parser.
        let head: String = prompt.chars().take(160).collect();
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": format!("ds-fake-models: unrecognised prompt: {head:?}") } })),
        )
            .into_response();
    };
    Json(completion(answer(task, &prompt))).into_response()
}

/// The answer for one task. Public so a test can assert what the fake would say without
/// going through HTTP.
pub fn answer(task: Task, prompt: &str) -> String {
    match task {
        // Counted, so successive pages read differently. Identical pages give the
        // extractor identical windows, and two identical claims collapse to one row by
        // content hash — true of the store, and a poor thing to build a count on.
        Task::PageParse => {
            static CALLS: AtomicUsize = AtomicUsize::new(0);
            let n = CALLS.fetch_add(1, Ordering::SeqCst) + 1;
            json!({
                "blocks": [
                    { "kind": "title", "bbox": [0.10, 0.05, 0.90, 0.10], "text": format!("Section {n}: retrieval quality"), "reading_order": 0 },
                    { "kind": "text", "bbox": [0.10, 0.15, 0.90, 0.25],
                      "text": format!("Supplier{n} Holdings reported a recall of 0.9{n} on the invoice set."), "reading_order": 1 },
                    { "kind": "table", "bbox": [0.10, 0.30, 0.90, 0.45],
                      "html": "<table><tr><td>Method</td><td>Acc</td></tr><tr><td>Ours</td><td>92.1</td></tr></table>", "reading_order": 2 }
                ]
            })
            .to_string()
        }
        // Regions and no text: that is the difference between a layout model and a
        // parser, and a layout step that received text would never notice it was given
        // the wrong answer.
        Task::Layout => json!({
            "regions": [
                { "kind": "title", "bbox": [0.10, 0.05, 0.90, 0.10] },
                { "kind": "text", "bbox": [0.10, 0.15, 0.90, 0.25] },
                { "kind": "table", "bbox": [0.10, 0.30, 0.90, 0.45] }
            ]
        })
        .to_string(),
        Task::PageClassify => json!({
            "doc_type": "report",
            "doc_type_confidence": 0.88,
            "p_handwriting": 0.02,
            "p_tables": 0.80,
            "p_formulas": 0.05,
            "p_charts": 0.05,
            "p_images": 0.10,
        })
        .to_string(),
        Task::DocumentClassify => json!({
            "doc_type": "report",
            "doc_type_confidence": 0.90,
            "p_handwriting": 0.02,
            "p_tables": 0.70,
            "p_formulas": 0.05,
            "p_charts": 0.05,
            "p_images": 0.10,
        })
        .to_string(),
        // These three answer in their own format rather than JSON, because their prompts
        // ask for exactly that: bare text, one `<table>`, bare LaTeX.
        Task::RegionOcr => "Supplier Holdings reported a recall of 0.92 on the invoice set.".to_string(),
        Task::Table => "<table><tr><td>Method</td><td>Acc</td></tr><tr><td>Ours</td><td>92.1</td></tr></table>".to_string(),
        Task::Formula => "E = mc^{2}".to_string(),
        Task::FigureDescribe => json!({ "description": "A bar chart comparing recall across three methods." }).to_string(),
        Task::ChartToTable => json!({
            "description": "Recall by method.",
            "html": "<table><tr><td>Method</td><td>Recall</td></tr><tr><td>Ours</td><td>0.92</td></tr></table>",
        })
        .to_string(),
        Task::Extract => extraction(prompt).to_string(),
        Task::Verify => verdicts(prompt).to_string(),
    }
}

/// Every text part of every message, joined — the prompt as the model would read it.
fn prompt_text(req: &Value) -> String {
    req["messages"]
        .as_array()
        .map(|ms| {
            ms.iter()
                .filter_map(|m| match &m["content"] {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(parts) => Some(parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// An extraction that *grounds*: the quote is copied out of the block the prompt showed,
/// so `ds_extract`'s grounding keeps it. A plausible-looking constant would be dropped as
/// ungrounded and the test would pass while asserting nothing.
fn extraction(prompt: &str) -> Value {
    let Some((label, body)) = first_block(prompt) else {
        return json!({ "entities": [], "assertions": [] });
    };
    let quote: String = body.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
    let name = body.split_whitespace().next().unwrap_or("Thing").trim_end_matches([',', '.']).to_string();
    json!({
        "entities": [{
            "id": "e1",
            "name": name,
            "type": "organisation",
            "mentions": [{ "block": label, "quote": quote }],
        }],
        "assertions": [
            {
                "predicate": "mentions_metric",
                "kind": "attribute",
                "args": [
                    { "role": "subject", "entity": "e1", "literal": null },
                    { "role": "value", "entity": null, "literal": { "value": quote, "unit": null } },
                ],
                "qualifiers": { "negated": false, "hedged": false, "condition": null, "time": null },
                "evidence": [{ "block": label, "quote": quote }],
                "confidence": 0.9,
            },
            // Deliberately identical in every window: same predicate, same literal, no
            // entity arg. The store is content-addressed, so every window's copy must
            // land as one row.
            {
                "predicate": "document_is_about",
                "kind": "attribute",
                "args": [{ "role": "value", "entity": null, "literal": { "value": "invoices", "unit": null } }],
                "qualifiers": { "negated": false, "hedged": false, "condition": null, "time": null },
                "evidence": [{ "block": label, "quote": quote }],
                "confidence": 0.7,
            },
        ],
    })
}

/// `[B1] the block's text` — the format `ds_extract::window::render_window` produces.
///
/// Scanned rather than read off line 1: the rendered window opens with `Document:` and
/// `Section:` headers, so anything insisting on a label at the top finds nothing.
fn first_block(prompt: &str) -> Option<(String, String)> {
    prompt.lines().find_map(|line| {
        let rest = line.trim().strip_prefix('[')?;
        let (label, body) = rest.split_once("] ")?;
        (label.starts_with('B') && !body.trim().is_empty()).then(|| (label.to_string(), body.trim().to_string()))
    })
}

/// Supports every assertion it is shown. The ids come out of the prompt, which is the
/// whole contract: a verdict for an id the extractor never emitted is ignored, so a
/// hard-coded id would verify nothing.
fn verdicts(prompt: &str) -> Value {
    let ids: Vec<&str> = prompt.lines().filter_map(|l| l.trim().strip_prefix("- id: ")).map(str::trim).collect();
    json!({ "verdicts": ids.iter().map(|id| json!({ "id": id, "supported": true })).collect::<Vec<_>>() })
}

async fn embeddings(Json(req): Json<Value>) -> Json<Value> {
    let n = match &req["input"] {
        Value::Array(a) => a.len(),
        _ => 1,
    };
    // Distinct per index, so a mix-up between two embed targets is visible rather than
    // hidden behind identical points.
    let data: Vec<Value> = (0..n)
        .map(|i| {
            let v: Vec<f32> = (0..DIMS).map(|d| ((i + d) % 7) as f32 / 7.0 + 0.01).collect();
            json!({ "embedding": v, "index": i, "object": "embedding" })
        })
        .collect();
    Json(json!({ "object": "list", "data": data, "usage": { "prompt_tokens": n * 8, "total_tokens": n * 8 } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_prompt_the_suite_ships_is_recognised() {
        // The marker strings are copied from the prompt files, so this is the test that
        // catches a prompt being reworded out from under the fake — which otherwise
        // shows up as a parser failure in an unrelated crate.
        for (file, want) in [
            ("page_parse.md", Task::PageParse),
            ("layout_detect.md", Task::Layout),
            ("page_classify.md", Task::PageClassify),
            ("document_classify.md", Task::DocumentClassify),
            ("region_ocr.md", Task::RegionOcr),
            ("table_recognize.md", Task::Table),
            ("formula_recognize.md", Task::Formula),
            ("figure_describe.md", Task::FigureDescribe),
            ("chart_to_table.md", Task::ChartToTable),
        ] {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ds-models/prompts/");
            let body = std::fs::read_to_string(format!("{path}{file}")).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(task_of(&body), Some(want), "{file} is no longer recognised by the fake");
        }
        for (file, want) in [("extract.md", Task::Extract), ("verify.md", Task::Verify)] {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ds-extract/prompts/");
            let body = std::fs::read_to_string(format!("{path}{file}")).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(task_of(&body), Some(want), "{file} is no longer recognised by the fake");
        }
        assert_eq!(task_of("something else entirely"), None);
    }

    #[test]
    fn extraction_quotes_the_block_it_was_shown() {
        let prompt = "You extract structured knowledge from one window.\nDocument: d\n[B7] Acme Holdings reported a recall of 0.92 here.";
        let v = extraction(prompt);
        let quote = v["assertions"][0]["evidence"][0]["quote"].as_str().expect("a quote");
        assert!(prompt.contains(quote), "the quote must be verbatim from the window, got {quote:?}");
        assert_eq!(v["assertions"][0]["evidence"][0]["block"], json!("B7"));
        assert_eq!(v["entities"][0]["name"], json!("Acme"));
    }

    #[test]
    fn verification_answers_only_the_ids_it_was_given() {
        let v = verdicts("You check whether assertions extracted\n- id: a1\n- id: a2\n");
        let ids: Vec<&str> = v["verdicts"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a1", "a2"]);
        assert!(verdicts("no ids here")["verdicts"].as_array().unwrap().is_empty());
    }

    /// A page-sized request must be accepted.
    ///
    /// The regression this pins cost a whole template: on a scanned corpus every
    /// `conditioned-e2e` candidate failed with `413 length limit exceeded`, which the
    /// client reports as "model call failed" and which therefore reads as a model
    /// problem. Axum's default body limit is 2 MB; a 200-dpi page is about 1 MB of PNG,
    /// base64 adds a third, and a conditioned page is a re-encode that can be larger
    /// still. A real VLM server takes multi-megabyte images, so the default made this
    /// double *stricter than production*.
    #[tokio::test]
    async fn a_page_sized_request_is_not_refused() {
        let (base, _handle) = spawn().await;
        // Four megabytes of base64, which is past the default and well within what a
        // page actually costs.
        let image = "A".repeat(4 * 1024 * 1024);
        let body = json!({
            "model": "paddleocr-vl-1.5",
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "Parse this document page completely." },
                { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{image}") } }
            ]}]
        });
        let res = reqwest::Client::new()
            .post(format!("{base}/chat/completions"))
            .json(&body)
            .send()
            .await
            .expect("the fake tier answers");
        assert_eq!(res.status(), 200, "a page-sized request must not be refused: {:?}", res.text().await);
    }

    #[test]
    fn layout_returns_regions_without_text() {
        let v: Value = serde_json::from_str(&answer(Task::Layout, "")).expect("json");
        let regions = v["regions"].as_array().expect("regions");
        assert!(!regions.is_empty());
        assert!(regions.iter().all(|r| r["text"].is_null()), "a layout model reports regions, not their text");
    }
}
