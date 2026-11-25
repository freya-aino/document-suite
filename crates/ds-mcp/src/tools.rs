//! The tools this server advertises, and what each one does.
//!
//! Written for a model to read, not as a transcription of the REST API. Three rules shaped
//! the catalogue:
//!
//! 1. **A tool answers a question someone asks, not an endpoint.** `get_document_text`
//!    fetches several page results and returns the text in reading order, because "what
//!    does this document say" is the question; a model handed raw `PageResult` JSON spends
//!    its context on bounding boxes.
//! 2. **Nothing irreversible is advertised.** Erasing a person, revoking a session and
//!    changing the active DAG list are all in the API and none is a tool here: a model
//!    should not be able to erase somebody because a prompt said so. Sharing *is* a tool,
//!    because it is reversible and it is the thing an assistant genuinely needs.
//! 3. **Every answer is bounded.** A 300-page document's text would fill any context
//!    window, so page ranges and byte caps are arguments with limits rather than hopes,
//!    and a truncated answer says that it was truncated.
//!
//! What is *not* here and cannot be: search. Retrieval reads Postgres and Qdrant directly
//! (`docs/STORES.md`) and there is no search endpoint on the API, so a search tool would
//! mean this service connecting to both stores and doing its own access control — a
//! different service with a different security story. `SPEC.md` says so plainly rather
//! than leaving a model to discover the gap.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::jsonrpc::RpcError;
use crate::suite::{truncate, Suite, SuiteError};

/// How a tool behaves, for the client's benefit (MCP `annotations`).
///
/// Hints, not guarantees — the specification is explicit that a client must not make
/// security decisions from them. They are here because they are what lets a client ask
/// "are you sure?" for the two tools that change who can read a document, and not for the
/// seven that only look.
#[derive(Debug, Clone, Copy)]
pub struct Behaviour {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

impl Behaviour {
    const fn read_only() -> Self {
        Self { read_only: true, destructive: false, idempotent: true }
    }

    const fn writes() -> Self {
        Self { read_only: false, destructive: false, idempotent: false }
    }

    const fn idempotent_write() -> Self {
        Self { read_only: false, destructive: false, idempotent: true }
    }

    fn to_json(self) -> Value {
        json!({
            "readOnlyHint": self.read_only,
            "destructiveHint": self.destructive,
            "idempotentHint": self.idempotent,
            // Every tool here reaches a service over the network, whose state other
            // people change.
            "openWorldHint": true,
        })
    }
}

pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub behaviour: Behaviour,
    pub schema: fn() -> Value,
}

/// A JSON Schema object with no extra properties allowed.
///
/// `additionalProperties: false` on purpose: a model that invents an argument should be
/// told rather than have it ignored, which is the same reasoning the suite applies to a
/// DAG parameter it does not honour.
fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn document_hk_property() -> Value {
    json!({
        "type": "string",
        "pattern": "^[0-9a-f]{64}$",
        "description": "The document's hash key: 64 lowercase hex characters, as returned by upload_document or list_documents.",
    })
}

/// Every tool, in a stable order.
///
/// `tools/list` returns this as it stands; the order is alphabetical within read tools and
/// then write tools, so a client diffing two listings sees a real change rather than a
/// reordering.
pub fn catalogue() -> &'static [Tool] {
    &CATALOGUE
}

/// The catalogue itself. A `static` rather than a value built per call: it is constant,
/// and `tools/list` is on the hot path of every session's first second.
static CATALOGUE: [Tool; 12] = [
        Tool {
            name: "list_documents",
            title: "List readable documents",
            description: "Lists the documents the calling person may read in their organization, newest first. \
                          Use this to find a document's hash key before calling get_document, get_document_text or share_document. \
                          A document is private to whoever uploaded it unless it has been shared, so this list is the caller's \
                          view and not the organization's whole corpus.",
            behaviour: Behaviour::read_only(),
            schema: || {
                object(
                    json!({
                        "page": { "type": "integer", "minimum": 0, "default": 0, "description": "0-based page of results." },
                        "page_size": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50, "description": "Results per page." },
                    }),
                    &[],
                )
            },
        },
        Tool {
            name: "get_document",
            title: "Get a document's summary",
            description: "Returns what the pipeline extracted from one document: page count, each page's class and the \
                          workflow that ran on it, how many blocks of each kind were found, and how many entities and \
                          assertions. Cheap, and the right first call on an unfamiliar document — it says whether the \
                          document is finished and worth reading before any text is fetched. For the text itself use \
                          get_document_text.",
            behaviour: Behaviour::read_only(),
            schema: || {
                object(
                    json!({
                        "document_hk": document_hk_property(),
                        "run_id": { "type": "string", "description": "A specific processing run (UUID). Omit for the current one." },
                    }),
                    &["document_hk"],
                )
            },
        },
        Tool {
            name: "get_document_text",
            title: "Read a document's text",
            description: "Returns the text of a page range in reading order, block by block, with each block's kind \
                          (paragraph, heading, table, caption…) and page number. This is the tool to use to read a \
                          document. Tables arrive as HTML and formulas as LaTeX, because that is how they are stored. \
                          Fetching is one API call per page, so ask for the pages you need rather than a whole long \
                          document, and the answer says plainly when it was truncated.",
            behaviour: Behaviour::read_only(),
            schema: || {
                object(
                    json!({
                        "document_hk": document_hk_property(),
                        "first_page": { "type": "integer", "minimum": 1, "default": 1, "description": "First page to read, 1-based." },
                        "pages": { "type": "integer", "minimum": 1, "description": "How many pages to read from first_page. Capped by the server." },
                        "run_id": { "type": "string", "description": "A specific processing run (UUID). Omit for the current one." },
                        "include_furniture": {
                            "type": "boolean",
                            "default": false,
                            "description": "Include running headers, footers and page numbers. Off by default: they repeat on every page and say nothing.",
                        },
                    }),
                    &["document_hk"],
                )
            },
        },
        Tool {
            name: "get_document_access",
            title: "See who can read a document",
            description: "Returns how widely a document is shared — private to its uploader, its department, a \
                          workspace, or the whole organization — and any individual grants on top of that. Use it before \
                          sharing, to see what the current state is, and after a refusal, to explain why.",
            behaviour: Behaviour::read_only(),
            schema: || object(json!({ "document_hk": document_hk_property() }), &["document_hk"]),
        },
        Tool {
            name: "get_job",
            title: "Check a processing job",
            description: "Returns one job's state and page progress. States are received, ingesting, pages_processing, \
                          stitching, doc_processing, writing, and then done, partial (finished with some pages failed) \
                          or failed. Use wait_for_job instead of polling this in a loop.",
            behaviour: Behaviour::read_only(),
            schema: || {
                object(
                    json!({ "job_id": { "type": "string", "description": "The job id (UUID) returned by upload_document." } }),
                    &["job_id"],
                )
            },
        },
        Tool {
            name: "get_batch",
            title: "Check a batch of uploads",
            description: "Returns a batch's progress: how many of its jobs are in each state, and a page of the jobs \
                          themselves. A batch is visible to the person who created it.",
            behaviour: Behaviour::read_only(),
            schema: || {
                object(
                    json!({
                        "batch_id": { "type": "string", "description": "The batch id (UUID) returned by upload_batch." },
                        "page": { "type": "integer", "minimum": 0, "default": 0, "description": "0-based page of the batch's jobs." },
                        "page_size": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50, "description": "Jobs per page." },
                    }),
                    &["batch_id"],
                )
            },
        },
        Tool {
            name: "wait_for_job",
            title: "Wait for a job to finish",
            description: "Polls a job until it reaches done, partial or failed, or until the wait runs out, and returns \
                          its final state. Use this after upload_document instead of calling get_job repeatedly: it \
                          costs one request every few seconds rather than as fast as the model can ask, and the \
                          organization's request budget is shared. A document of a few pages is usually finished in \
                          seconds to a minute; a long one can take much longer, and a timed-out wait is not a failure — \
                          the job keeps running and can be waited on again.",
            behaviour: Behaviour::read_only(),
            schema: || {
                object(
                    json!({
                        "job_id": { "type": "string", "description": "The job id (UUID)." },
                        "timeout_seconds": { "type": "integer", "minimum": 1, "description": "How long to wait. Capped by the server." },
                    }),
                    &["job_id"],
                )
            },
        },
        Tool {
            name: "upload_document",
            title: "Submit a document for processing",
            description: "Submits a document that is already in the organization's object store and returns a job id. \
                          The object must be under originals/<organization>/ or inbox/<organization>/ — this tool \
                          cannot upload a local file, because the MCP server and the suite do not share a filesystem; \
                          put the file in the bucket first, or use the HTTP API's multipart upload directly. \
                          Processing is asynchronous: wait with wait_for_job, then read with get_document_text. \
                          The uploaded document is private to the caller unless api.default_visibility says otherwise.",
            behaviour: Behaviour::writes(),
            schema: || {
                object(
                    json!({
                        "source_uri": {
                            "type": "string",
                            "description": "Object key or s3:// URI under originals/<organization>/ or inbox/<organization>/.",
                        },
                        "language": {
                            "type": "string",
                            "description": "BCP-47 hint for the whole document, e.g. `en` or `de-CH`. Required: nothing detects it.",
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["batch", "interactive"],
                            "default": "batch",
                            "description": "`interactive` for a document someone is waiting on; it takes reserved capacity. Use `batch` for bulk work.",
                        },
                        "profile": {
                            "type": "string",
                            "enum": ["quality", "balanced", "fast"],
                            "default": "balanced",
                            "description": "Which end of the quality/cost trade to route for.",
                        },
                        "doc_type": {
                            "type": "string",
                            "description": "What the document is, if known (e.g. invoice, scientific_paper, form, report). Trusted over anything the classifier infers, and it reaches page routing, so it is worth setting when you know.",
                        },
                        "external_ref": { "type": "string", "description": "Your own identifier, stored with the document." },
                        "skip_if_processed": {
                            "type": "boolean",
                            "default": false,
                            "description": "Return the caller's existing run of this document instead of starting another, if there is one.",
                        },
                    }),
                    &["source_uri", "language"],
                )
            },
        },
        Tool {
            name: "upload_batch",
            title: "Submit several documents",
            description: "Submits many documents in one call and returns a batch id plus a job id per document. Every \
                          document is validated before any is accepted, so a bad entry fails the whole call rather than \
                          leaving half a batch running. Use this rather than calling upload_document in a loop: it is \
                          one request against the organization's rate limit instead of one per document.",
            behaviour: Behaviour::writes(),
            schema: || {
                object(
                    json!({
                        "documents": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 1000,
                            "description": "The documents to submit.",
                            "items": object(
                                json!({
                                    "source_uri": { "type": "string", "description": "Object key or s3:// URI under the organization's prefix." },
                                    "language": { "type": "string", "description": "Overrides the batch's language." },
                                    "doc_type": { "type": "string", "description": "Overrides the batch's doc_type, for a mixed batch." },
                                    "external_ref": { "type": "string", "description": "Your own identifier for this document." },
                                }),
                                &["source_uri"],
                            ),
                        },
                        "language": { "type": "string", "description": "Default BCP-47 hint for documents without their own." },
                        "priority": { "type": "string", "enum": ["batch", "interactive"], "default": "batch", "description": "`interactive` takes reserved capacity; use `batch` for bulk work." },
                        "profile": { "type": "string", "enum": ["quality", "balanced", "fast"], "default": "balanced", "description": "Which end of the quality/cost trade to route for." },
                        "doc_type": { "type": "string", "description": "Default document type for the batch." },
                    }),
                    &["documents"],
                )
            },
        },
        Tool {
            name: "share_document",
            title: "Change who can read a document",
            description: "Sets how widely a document is shared inside the organization: private (its uploader alone), \
                          department, workspace, or organization. Only the document's owner or an administrator may \
                          call it. Reversible — set it back to private — and it never shares outside the organization, \
                          which is not something this API can do at all. Prefer grant_access when one named person or \
                          team needs a document, rather than widening it for everyone.",
            behaviour: Behaviour::idempotent_write(),
            schema: || {
                object(
                    json!({
                        "document_hk": document_hk_property(),
                        "visibility": {
                            "type": "string",
                            "enum": ["private", "department", "workspace", "organization"],
                            "description": "private: the uploader and administrators. department: the uploader's department. workspace: anyone in one of the document's workspaces. organization: every member.",
                        },
                    }),
                    &["document_hk", "visibility"],
                )
            },
        },
        Tool {
            name: "grant_access",
            title: "Share a document with someone",
            description: "Grants one person, department or workspace access to a document without widening it for \
                          everyone else. Only the document's owner or an administrator may call it. Granting twice is \
                          one grant, and revoke_access undoes it.",
            behaviour: Behaviour::idempotent_write(),
            schema: || {
                object(
                    json!({
                        "document_hk": document_hk_property(),
                        "kind": {
                            "type": "string",
                            "enum": ["subject", "department", "workspace"],
                            "description": "What `id` names: a person (their identity-provider subject), a department, or a workspace.",
                        },
                        "id": { "type": "string", "minLength": 1, "maxLength": 256, "description": "The person, department or workspace to grant access to." },
                    }),
                    &["document_hk", "kind", "id"],
                )
            },
        },
        Tool {
            name: "revoke_access",
            title: "Withdraw a share",
            description: "Removes a grant made by grant_access. Only the document's owner or an administrator may call \
                          it. It does not change the document's visibility, so a document shared with the whole \
                          organization stays readable after a grant is withdrawn — use share_document for that.",
            behaviour: Behaviour::idempotent_write(),
            schema: || {
                object(
                    json!({
                        "document_hk": document_hk_property(),
                        "kind": { "type": "string", "enum": ["subject", "department", "workspace"], "description": "What `id` names." },
                        "id": { "type": "string", "minLength": 1, "maxLength": 256, "description": "The person, department or workspace whose grant to withdraw." },
                    }),
                    &["document_hk", "kind", "id"],
                )
            },
        },
];

/// The `tools/list` payload.
pub fn list() -> Value {
    let tools: Vec<Value> = catalogue()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": t.behaviour.to_json(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// What a tool call produced: a text block for the model, and the API's own JSON beside it
/// for a client that would rather parse than read.
pub struct Outcome {
    pub text: String,
    pub structured: Option<Value>,
}

impl Outcome {
    fn new(text: impl Into<String>, structured: Value) -> Self {
        Self { text: text.into(), structured: Some(structured) }
    }

    pub fn to_json(&self, is_error: bool) -> Value {
        let mut out = json!({
            "content": [{ "type": "text", "text": self.text }],
            "isError": is_error,
        });
        if let Some(s) = &self.structured {
            out["structuredContent"] = s.clone();
        }
        out
    }
}

/// Limits a deployment sets, so a model cannot ask for an unbounded amount of work.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Characters of document text one call may return.
    pub max_text_chars: usize,
    /// Pages one `get_document_text` call may fetch.
    pub max_pages: u32,
    /// Longest `wait_for_job` may wait.
    pub max_wait: std::time::Duration,
    /// Interval between polls in `wait_for_job`.
    pub poll_interval: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // ~15k tokens of text: enough for a long paper's worth of pages, and still
            // leaving a model room to think about it.
            max_text_chars: 60_000,
            max_pages: 20,
            max_wait: std::time::Duration::from_secs(120),
            poll_interval: std::time::Duration::from_secs(3),
        }
    }
}

/// Runs one tool.
///
/// `Err` is a **protocol** error — an unknown tool, or arguments that do not fit the
/// schema — which the specification says to report as a JSON-RPC error. Everything that
/// goes wrong while *doing* the work comes back as `Ok` with `isError` set, so the model
/// sees it and can act: ask the owner to share a document, wait out a rate limit, correct
/// an id.
pub async fn call(suite: &Suite, limits: &Limits, name: &str, args: &Value, token: Option<&str>) -> Result<(Outcome, bool), RpcError> {
    let args = if args.is_null() { json!({}) } else { args.clone() };
    let result = match name {
        "list_documents" => list_documents(suite, &args, token).await,
        "get_document" => get_document(suite, &args, token).await,
        "get_document_text" => get_document_text(suite, limits, &args, token).await,
        "get_document_access" => get_document_access(suite, &args, token).await,
        "get_job" => get_job(suite, &args, token).await,
        "get_batch" => get_batch(suite, &args, token).await,
        "wait_for_job" => wait_for_job(suite, limits, &args, token).await,
        "upload_document" => upload_document(suite, &args, token).await,
        "upload_batch" => upload_batch(suite, &args, token).await,
        "share_document" => share_document(suite, &args, token).await,
        "grant_access" => grant(suite, &args, token, true).await,
        "revoke_access" => grant(suite, &args, token, false).await,
        // The specification's own example for an unknown tool is -32602.
        other => return Err(RpcError::invalid_params(format!("unknown tool {other:?}"))),
    };
    match result {
        Ok(outcome) => Ok((outcome, false)),
        Err(ToolFailure::Protocol(e)) => Err(e),
        Err(ToolFailure::Suite(e)) => {
            metrics::counter!("mcp_tool_errors_total", "tool" => name.to_string()).increment(1);
            Ok((
                Outcome {
                    // The suite's own message, then what to do about it. A model told only
                    // that something failed will retry the same call.
                    text: format!("{e}\n\n{}", e.advice()),
                    structured: None,
                },
                true,
            ))
        }
    }
}

/// Separates the two kinds of failure so `call` can route them to the two places the
/// specification puts them.
#[derive(Debug)]
enum ToolFailure {
    Protocol(RpcError),
    Suite(SuiteError),
}

impl From<SuiteError> for ToolFailure {
    fn from(e: SuiteError) -> Self {
        ToolFailure::Suite(e)
    }
}

impl From<RpcError> for ToolFailure {
    fn from(e: RpcError) -> Self {
        ToolFailure::Protocol(e)
    }
}

type Done = Result<Outcome, ToolFailure>;

/// Arguments, parsed into a type that refuses what the schema refuses.
fn parse_args<T: serde::de::DeserializeOwned>(tool: &str, args: &Value) -> Result<T, ToolFailure> {
    serde_json::from_value(args.clone())
        .map_err(|e| ToolFailure::Protocol(RpcError::invalid_params(format!("{tool}: {e}")).with_data(json!({ "tool": tool }))))
}

// ───────────── read tools ─────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    page: u32,
    page_size: Option<u32>,
}

async fn list_documents(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: ListArgs = parse_args("list_documents", args)?;
    let size = a.page_size.unwrap_or(50).clamp(1, 500);
    let body: Value = suite.call(reqwest::Method::GET, &format!("/v1/documents?page={}&page_size={size}", a.page), None, token).await?;
    let docs = body["documents"].as_array().cloned().unwrap_or_default();
    let mut text = format!("{} document(s) readable by this caller (page {}).\n", docs.len(), a.page);
    for d in &docs {
        let hk = d["document_hk"].as_str().unwrap_or("?");
        let visibility = d["visibility"].as_str().unwrap_or("?");
        let created = d["created_at"].as_str().unwrap_or("?");
        let own = if d["owner_subject"].is_null() { " (the organization's)" } else { "" };
        text.push_str(&format!("- {hk} · {visibility}{own} · uploaded {created}\n"));
    }
    if docs.is_empty() {
        text.push_str("Nothing is readable by this caller yet. A document is private to whoever uploaded it until it is shared.\n");
    }
    Ok(Outcome::new(text, body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentArgs {
    document_hk: String,
    run_id: Option<String>,
}

async fn get_document(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: DocumentArgs = parse_args("get_document", args)?;
    let hk = hash_key(&a.document_hk)?;
    let path = match &a.run_id {
        Some(r) => format!("/v1/documents/{hk}?run_id={r}"),
        None => format!("/v1/documents/{hk}"),
    };
    let body: Value = suite.call(reqwest::Method::GET, &path, None, token).await?;
    let pages = body["page_count"].as_i64().unwrap_or(0);
    let state = body["run_state"].as_str().unwrap_or("unknown");
    let mut text = format!(
        "Document {hk}\n  run {} · state {state} · {pages} page(s)\n  entities: {} · assertions: {}\n",
        body["run_id"].as_str().unwrap_or("?"),
        body["entities"].as_i64().unwrap_or(0),
        body["assertions"].as_i64().unwrap_or(0),
    );
    if let Some(kinds) = body["blocks_by_kind"].as_object() {
        let mut parts: Vec<String> = kinds.iter().map(|(k, v)| format!("{k} {}", v.as_i64().unwrap_or(0))).collect();
        parts.sort();
        text.push_str(&format!("  blocks: {}\n", parts.join(", ")));
    }
    if let Some(ps) = body["pages"].as_array() {
        for p in ps.iter().take(10) {
            let profile = &p["profile"];
            text.push_str(&format!(
                "  page {}: {} · {} · workflow {}\n",
                p["page_no"].as_i64().unwrap_or(0),
                profile["doc_type"].as_str().unwrap_or("?"),
                if profile["born_digital"].as_bool().unwrap_or(false) { "born-digital" } else { "scanned" },
                p["dag_ref"].as_str().unwrap_or("?"),
            ));
        }
        if ps.len() > 10 {
            text.push_str(&format!("  … and {} more page(s)\n", ps.len() - 10));
        }
    }
    if state == "writing" {
        text.push_str("\nThis run is still being written: its vectors may be missing, though the text and facts above are complete.\n");
    }
    Ok(Outcome::new(text, body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    document_hk: String,
    first_page: Option<u32>,
    pages: Option<u32>,
    run_id: Option<String>,
    #[serde(default)]
    include_furniture: bool,
}

async fn get_document_text(suite: &Suite, limits: &Limits, args: &Value, token: Option<&str>) -> Done {
    let a: TextArgs = parse_args("get_document_text", args)?;
    let hk = hash_key(&a.document_hk)?;
    let first = a.first_page.unwrap_or(1).max(1);
    let wanted = a.pages.unwrap_or(limits.max_pages).clamp(1, limits.max_pages);

    let mut text = String::new();
    let mut blocks_out = Vec::new();
    let mut truncated = None;
    let mut read = 0u32;

    for page_no in first..first + wanted {
        let path = match &a.run_id {
            Some(r) => format!("/v1/documents/{hk}/pages/{page_no}?run_id={r}"),
            None => format!("/v1/documents/{hk}/pages/{page_no}"),
        };
        let raw = match suite.call_raw(reqwest::Method::GET, &path, None, token).await {
            Ok(raw) => raw,
            // A page past the end of the document, or one that failed: the pages before
            // it are still an answer, and saying where it stopped is more useful than
            // failing the call.
            Err(SuiteError::NotFound(_)) if read > 0 => break,
            Err(e) => return Err(e.into()),
        };
        let page: Value = serde_json::from_str(&raw).map_err(|e| SuiteError::Malformed(e.to_string()))?;
        read += 1;
        let mut blocks: Vec<&Value> = page["blocks"].as_array().map(|b| b.iter().collect()).unwrap_or_default();
        blocks.sort_by_key(|b| b["reading_order"].as_u64().unwrap_or(u64::MAX));
        text.push_str(&format!("\n--- page {page_no} ---\n"));
        for b in blocks {
            let kind = b["kind"].as_str().unwrap_or("other");
            if !a.include_furniture && matches!(kind, "header" | "footer" | "page_number") {
                continue;
            }
            let body = block_text(b);
            if body.trim().is_empty() {
                continue;
            }
            let line = format!("[{kind}] {body}\n");
            if text.chars().count() + line.chars().count() > limits.max_text_chars {
                truncated = Some(page_no);
                break;
            }
            text.push_str(&line);
            blocks_out.push(json!({ "page_no": page_no, "kind": kind, "text": body }));
        }
        if truncated.is_some() {
            break;
        }
    }

    let last = first + read.saturating_sub(1);
    let mut header = format!("Document {hk}, pages {first}–{last}");
    if let Some(at) = truncated {
        header.push_str(&format!(
            " (truncated at page {at}: the answer reached the server's {} character limit. Ask for a later first_page to continue.)",
            limits.max_text_chars
        ));
    }
    if read == 0 {
        header.push_str(" — no pages were returned. The document may have fewer pages, or this run may not have finished.");
    }
    Ok(Outcome::new(format!("{header}\n{text}"), json!({ "document_hk": hk, "first_page": first, "last_page": last, "truncated_at_page": truncated, "blocks": blocks_out })))
}

/// A block's text, in whatever form it was stored: tables as HTML, formulas as LaTeX,
/// figures as their description. Falling back to `text` keeps a block that has only the
/// plain projection.
fn block_text(b: &Value) -> String {
    let content = &b["content"];
    let from_content = match content["type"].as_str() {
        Some("table") => content["html"].as_str(),
        Some("formula") => content["latex"].as_str(),
        Some("figure") | Some("chart") => content["description"].as_str(),
        _ => None,
    };
    from_content.or_else(|| b["text"].as_str()).unwrap_or_default().trim().to_string()
}

async fn get_document_access(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: DocumentArgs = parse_args("get_document_access", args)?;
    let hk = hash_key(&a.document_hk)?;
    let body: Value = suite.call(reqwest::Method::GET, &format!("/v1/documents/{hk}/access"), None, token).await?;
    let visibility = body["visibility"].as_str().unwrap_or("?");
    let mut text = format!("Document {hk} is shared: {visibility}.\n");
    if body["legacy"].as_bool().unwrap_or(false) {
        text.push_str("It predates per-person scoping, so it has no recorded owner and the whole organization can read it.\n");
    }
    match body["owner"].as_str() {
        Some(owner) => text.push_str(&format!("Owner: {owner}.\n")),
        None => text.push_str("The owner is not disclosed to this caller.\n"),
    }
    match body["grants"].as_array() {
        Some(g) if !g.is_empty() => {
            text.push_str("Grants:\n");
            for grant in g {
                text.push_str(&format!("- {} {}\n", grant["kind"].as_str().unwrap_or("?"), grant["id"].as_str().unwrap_or("?")));
            }
        }
        _ => text.push_str("No individual grants.\n"),
    }
    Ok(Outcome::new(text, body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobArgs {
    job_id: String,
}

async fn get_job(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: JobArgs = parse_args("get_job", args)?;
    let id = uuid(&a.job_id, "job_id")?;
    let body: Value = suite.call(reqwest::Method::GET, &format!("/v1/jobs/{id}"), None, token).await?;
    Ok(Outcome::new(job_text(&body), body))
}

fn job_text(body: &Value) -> String {
    let state = body["state"].as_str().unwrap_or("?");
    let mut text = format!("Job {} is {state}.\n", body["job_id"].as_str().unwrap_or("?"));
    if let Some(hk) = body["document_hk"].as_str() {
        text.push_str(&format!("Document: {hk}\n"));
    }
    let (done, failed) = (body["pages_done"].as_i64().unwrap_or(0), body["pages_failed"].as_i64().unwrap_or(0));
    if let Some(total) = body["pages_total"].as_i64() {
        text.push_str(&format!("Pages: {done} done, {failed} failed, of {total}.\n"));
    }
    if let Some(e) = body["error"].as_str() {
        text.push_str(&format!("Error: {e}\n"));
    }
    match state {
        "done" => text.push_str("Finished. The text can be read with get_document_text.\n"),
        "partial" => text.push_str("Finished with some pages failed — the rest is readable, and get_document says which pages exist.\n"),
        "failed" => text.push_str("This job will not finish on its own. Re-submitting the same document resumes it rather than repeating the work.\n"),
        _ => text.push_str("Still running.\n"),
    }
    text
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchArgs {
    batch_id: String,
    #[serde(default)]
    page: u32,
    page_size: Option<u32>,
}

async fn get_batch(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: BatchArgs = parse_args("get_batch", args)?;
    let id = uuid(&a.batch_id, "batch_id")?;
    let size = a.page_size.unwrap_or(50).clamp(1, 500);
    let body: Value = suite.call(reqwest::Method::GET, &format!("/v1/batches/{id}?page={}&page_size={size}", a.page), None, token).await?;
    let mut counts: Vec<String> =
        body["counts"].as_object().map(|c| c.iter().map(|(k, v)| format!("{k} {}", v.as_i64().unwrap_or(0))).collect()).unwrap_or_default();
    counts.sort();
    let text = format!(
        "Batch {id}: {} document(s), {} job(s).\nStates: {}\n",
        body["documents"].as_i64().unwrap_or(0),
        body["total_jobs"].as_i64().unwrap_or(0),
        if counts.is_empty() { "none yet".to_string() } else { counts.join(", ") }
    );
    Ok(Outcome::new(text, body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    job_id: String,
    timeout_seconds: Option<u64>,
}

async fn wait_for_job(suite: &Suite, limits: &Limits, args: &Value, token: Option<&str>) -> Done {
    let a: WaitArgs = parse_args("wait_for_job", args)?;
    let id = uuid(&a.job_id, "job_id")?;
    let budget = a
        .timeout_seconds
        .map(std::time::Duration::from_secs)
        .unwrap_or(limits.max_wait)
        .min(limits.max_wait);
    let deadline = tokio::time::Instant::now() + budget;
    let mut polls = 0u32;
    loop {
        let body: Value = suite.call(reqwest::Method::GET, &format!("/v1/jobs/{id}"), None, token).await?;
        polls += 1;
        let state = body["state"].as_str().unwrap_or("").to_string();
        if matches!(state.as_str(), "done" | "partial" | "failed") {
            let text = format!("{}\nWaited {polls} poll(s).\n", job_text(&body));
            return Ok(Outcome::new(text, body));
        }
        if tokio::time::Instant::now() + limits.poll_interval >= deadline {
            let text = format!(
                "{}\nStill running after {}s ({polls} poll(s)). This is not a failure: the job keeps going, and \
                 wait_for_job can be called again.\n",
                job_text(&body),
                budget.as_secs()
            );
            return Ok(Outcome::new(text, body));
        }
        tokio::time::sleep(limits.poll_interval).await;
    }
}

// ───────────── write tools ─────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadArgs {
    source_uri: String,
    language: String,
    priority: Option<String>,
    profile: Option<String>,
    doc_type: Option<String>,
    external_ref: Option<String>,
    #[serde(default)]
    skip_if_processed: bool,
}

async fn upload_document(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: UploadArgs = parse_args("upload_document", args)?;
    let mut body = json!({ "language": a.language, "source_uri": a.source_uri });
    for (key, value) in [("priority", a.priority), ("profile", a.profile), ("doc_type", a.doc_type), ("external_ref", a.external_ref)] {
        if let Some(v) = value {
            body[key] = json!(v);
        }
    }
    let path = if a.skip_if_processed { "/v1/documents?skip_if_processed=true" } else { "/v1/documents" };
    let out: Value = suite.call(reqwest::Method::POST, path, Some(body), token).await?;
    let job = out["job_id"].as_str().unwrap_or("?");
    let text = format!(
        "Accepted as job {job}.{}\nWait for it with wait_for_job, then read it with get_document_text.\n",
        match out["document_hk"].as_str() {
            Some(hk) => format!(" Document {hk}."),
            None => " The document's hash key is known once ingest has read the file; get_job will have it.".into(),
        }
    );
    Ok(Outcome::new(text, out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchUploadArgs {
    documents: Vec<Value>,
    language: Option<String>,
    priority: Option<String>,
    profile: Option<String>,
    doc_type: Option<String>,
}

async fn upload_batch(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: BatchUploadArgs = parse_args("upload_batch", args)?;
    let mut options = json!({});
    for (key, value) in [("language", a.language), ("priority", a.priority), ("profile", a.profile), ("doc_type", a.doc_type)] {
        if let Some(v) = value {
            options[key] = json!(v);
        }
    }
    let body = json!({ "options": options, "documents": a.documents });
    let out: Value = suite.call(reqwest::Method::POST, "/v1/batches", Some(body), token).await?;
    let jobs = out["job_ids"].as_array().map(|j| j.len()).unwrap_or(0);
    let failed = out["failed_job_ids"].as_array().map(|j| j.len()).unwrap_or(0);
    let mut text = format!("Batch {} accepted with {jobs} job(s).\n", out["batch_id"].as_str().unwrap_or("?"));
    if failed > 0 {
        text.push_str(&format!("{failed} job(s) could not be started and are marked failed.\n"));
    }
    text.push_str("Follow it with get_batch.\n");
    Ok(Outcome::new(text, out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareArgs {
    document_hk: String,
    visibility: String,
}

async fn share_document(suite: &Suite, args: &Value, token: Option<&str>) -> Done {
    let a: ShareArgs = parse_args("share_document", args)?;
    let hk = hash_key(&a.document_hk)?;
    if !matches!(a.visibility.as_str(), "private" | "department" | "workspace" | "organization") {
        return Err(ToolFailure::Protocol(RpcError::invalid_params(format!(
            "share_document: visibility must be private, department, workspace or organization, not {:?}",
            a.visibility
        ))));
    }
    let out: Value =
        suite.call(reqwest::Method::PUT, &format!("/v1/documents/{hk}/access"), Some(json!({ "visibility": a.visibility })), token).await?;
    let text = format!("Document {hk} is now shared: {}.\nget_document_access shows the grants on top of that.\n", a.visibility);
    Ok(Outcome::new(text, out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantArgs {
    document_hk: String,
    kind: String,
    id: String,
}

async fn grant(suite: &Suite, args: &Value, token: Option<&str>, add: bool) -> Done {
    let tool = if add { "grant_access" } else { "revoke_access" };
    let a: GrantArgs = parse_args(tool, args)?;
    let hk = hash_key(&a.document_hk)?;
    if !matches!(a.kind.as_str(), "subject" | "department" | "workspace") {
        return Err(ToolFailure::Protocol(RpcError::invalid_params(format!(
            "{tool}: kind must be subject, department or workspace, not {:?}",
            a.kind
        ))));
    }
    if a.id.trim().is_empty() {
        return Err(ToolFailure::Protocol(RpcError::invalid_params(format!("{tool}: id must not be empty"))));
    }
    if add {
        let out: Value = suite
            .call(reqwest::Method::POST, &format!("/v1/documents/{hk}/grants"), Some(json!({ "kind": a.kind, "id": a.id })), token)
            .await?;
        Ok(Outcome::new(format!("{} {} can now read document {hk}.\n", a.kind, a.id), out))
    } else {
        // 204, so there is no body to parse.
        suite
            .call_raw(reqwest::Method::DELETE, &format!("/v1/documents/{hk}/grants/{}/{}", a.kind, urlencode(&a.id)), None, token)
            .await?;
        Ok(Outcome::new(
            format!("The grant to {} {} on document {hk} is withdrawn. Its visibility is unchanged.\n", a.kind, a.id),
            json!({ "document_hk": hk, "kind": a.kind, "id": a.id, "revoked": true }),
        ))
    }
}

// ───────────── argument helpers ─────────────

/// A document hash key, checked here rather than at the API: a model that mistypes one
/// should be told what the shape is, not handed a 400 about a path parameter.
fn hash_key(raw: &str) -> Result<String, ToolFailure> {
    let hk = raw.trim().to_ascii_lowercase();
    if hk.len() == 64 && hk.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(hk)
    } else {
        Err(ToolFailure::Protocol(RpcError::invalid_params(format!(
            "document_hk must be 64 hex characters (got {} character(s)). list_documents returns valid ones.",
            raw.trim().chars().count()
        ))))
    }
}

fn uuid(raw: &str, field: &str) -> Result<uuid::Uuid, ToolFailure> {
    raw.trim()
        .parse()
        .map_err(|_| ToolFailure::Protocol(RpcError::invalid_params(format!("{field} must be a UUID, got {:?}", truncate(raw, 60)))))
}

/// Percent-encodes a path segment. Only the characters that would change the path's
/// meaning: a grantee id is a subject, a department or a workspace name, and a `/` in one
/// would otherwise address a different route.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_description_a_schema_and_honest_annotations() {
        let listed = list();
        let tools = listed["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), catalogue().len());
        let mut names = std::collections::HashSet::new();
        for t in tools {
            let name = t["name"].as_str().expect("a name");
            assert!(names.insert(name), "{name} is listed twice");
            // A model chooses a tool by its description, so an empty or terse one is a
            // defect rather than a style question.
            let description = t["description"].as_str().expect("a description");
            assert!(description.len() > 80, "{name}: description too thin to choose by");
            assert!(t["title"].as_str().is_some_and(|s| !s.is_empty()));

            let schema = &t["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}");
            assert_eq!(schema["additionalProperties"], false, "{name}: an invented argument must be refused, not ignored");
            assert!(schema["properties"].is_object(), "{name}");
            // Every property is described, for the same reason the tool is.
            for (prop, spec) in schema["properties"].as_object().expect("properties") {
                assert!(spec["type"].is_string(), "{name}.{prop} has no type");
                assert!(spec["description"].is_string() || spec["enum"].is_array(), "{name}.{prop} is undocumented");
            }
            // Required arguments must exist as properties.
            for req in schema["required"].as_array().expect("required") {
                let key = req.as_str().expect("a name");
                assert!(schema["properties"].get(key).is_some(), "{name}: required {key} is not a property");
            }
            let a = &t["annotations"];
            assert!(a["readOnlyHint"].is_boolean() && a["destructiveHint"].is_boolean() && a["openWorldHint"] == true, "{name}");
        }
    }

    #[test]
    fn nothing_irreversible_is_advertised() {
        // Erasing a person, revoking a session and activating a DAG list are all in the
        // API. None of them is a tool: a model must not be able to erase somebody because
        // a prompt said so, and an administrative change to routing is not an assistant's
        // to make. `SPEC.md` records this as a decision rather than an oversight.
        let names: Vec<&str> = catalogue().iter().map(|t| t.name).collect();
        for forbidden in ["erase_owner", "revoke_token", "activate_dag_list", "upload_dag_list", "delete_document"] {
            assert!(!names.contains(&forbidden), "{forbidden} must not be advertised");
        }
        // And nothing claims to be destructive, because nothing here is.
        assert!(catalogue().iter().all(|t| !t.behaviour.destructive));
        // The tools that change who can read a document are not read-only, and the rest
        // are: a client that asks for confirmation needs that line to be honest.
        let writes: Vec<&str> = catalogue().iter().filter(|t| !t.behaviour.read_only).map(|t| t.name).collect();
        assert_eq!(writes, vec!["upload_document", "upload_batch", "share_document", "grant_access", "revoke_access"]);
    }

    #[test]
    fn a_hash_key_is_checked_here_so_the_model_is_told_the_shape() {
        assert_eq!(hash_key(&"A".repeat(64)).expect("hex is case-insensitive"), "a".repeat(64));
        let err = hash_key("abc").expect_err("too short");
        let ToolFailure::Protocol(e) = err else { panic!("a bad argument is a protocol error") };
        assert!(e.message.contains("64 hex"), "{}", e.message);
        assert!(e.message.contains("list_documents"), "the message should say where a good one comes from");
        assert!(hash_key(&"z".repeat(64)).is_err(), "not hex");
    }

    #[test]
    fn a_grantee_id_cannot_escape_its_path_segment() {
        assert_eq!(urlencode("alice"), "alice");
        assert_eq!(urlencode("a/b"), "a%2Fb", "a slash would address a different route");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("ü"), "%C3%BC");
    }

    #[test]
    fn a_block_is_rendered_in_the_form_it_was_stored_in() {
        let table = json!({ "kind": "table", "content": { "type": "table", "html": "<table><tr><td>1</td></tr></table>" }, "text": "1" });
        assert!(block_text(&table).starts_with("<table>"), "a table is more useful as HTML than as its flattened text");
        let formula = json!({ "kind": "formula", "content": { "type": "formula", "latex": "e^{i\\pi}" }, "text": "eipi" });
        assert_eq!(block_text(&formula), "e^{i\\pi}");
        let figure = json!({ "kind": "figure", "content": { "type": "figure", "crop_uri": "k", "description": "a bar chart" } });
        assert_eq!(block_text(&figure), "a bar chart");
        let para = json!({ "kind": "paragraph", "content": { "type": "text", "text": "hello" }, "text": "hello" });
        assert_eq!(block_text(&para), "hello");
        // A figure with no description falls back to the plain projection, and an empty
        // block renders as nothing rather than as "null".
        let bare = json!({ "kind": "figure", "content": { "type": "figure", "crop_uri": "k" }, "text": "" });
        assert_eq!(block_text(&bare), "");
    }

    #[test]
    fn job_text_says_what_to_do_next() {
        let done = job_text(&json!({ "job_id": "j", "state": "done", "document_hk": "a", "pages_done": 3, "pages_total": 3 }));
        assert!(done.contains("get_document_text"), "{done}");
        let failed = job_text(&json!({ "job_id": "j", "state": "failed", "error": "unsupported format", "pages_done": 0 }));
        assert!(failed.contains("unsupported format") && failed.contains("resumes"), "{failed}");
        let running = job_text(&json!({ "job_id": "j", "state": "pages_processing", "pages_done": 1, "pages_total": 9 }));
        assert!(running.contains("1 done") && running.contains("Still running"), "{running}");
    }

    #[test]
    fn the_limits_are_bounded_by_the_server_not_by_the_caller() {
        let l = Limits::default();
        assert!(l.max_pages > 0 && l.max_text_chars > 1000);
        assert!(l.poll_interval < l.max_wait, "a wait must poll more than once");
    }
}
