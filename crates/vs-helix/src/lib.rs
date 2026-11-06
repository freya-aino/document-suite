//! HelixDB writer. See `SPEC.md`.
//!
//! HelixDB is only called through the compiled HelixQL queries in `helix/db/queries.hx`
//! (`POST {base_url}/{QueryName}` with the query's parameters as a JSON object).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use vs_core::{HashKey, RunId, TenantId};
use vs_extract::resolve::{CorpusResolver, Resolution};
use vs_extract::{DocEntity, ExtractError};
use vs_graph::{DocGraph, EdgeKind, GEdge, GNode, NodeKind};
use vs_models::{ChatRequest, Embedder, OpenAiClient};

#[derive(Debug, thiserror::Error)]
pub enum HelixError {
    #[error("http: {0}")]
    Http(String),
    #[error("helix query {query} returned {status}: {body}")]
    Status { query: &'static str, status: u16, body: String },
    #[error("unexpected response from {query}: {reason}")]
    Response { query: &'static str, reason: String },
}

impl HelixError {
    /// A lookup by key found nothing (Helix 2.3.5 reports this as an error).
    pub fn is_not_found(&self) -> bool {
        matches!(self, HelixError::Status { body, .. } if body.contains("No value found"))
    }

    /// A create hit an existing key on a unique index.
    pub fn is_duplicate(&self) -> bool {
        matches!(self, HelixError::Status { body, .. } if body.contains("Duplicate key on unique index"))
    }
}

/// Embeddings keyed by `EmbedTarget::node_key`.
pub struct Vectors {
    pub model_ref: String,
    pub dimensions: usize,
    pub by_key: HashMap<String, Vec<f32>>,
}

/// What the graph does not carry about the run.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub tenant: TenantId,
    pub run_id: RunId,
    pub language: String,
    pub page_count: u32,
    pub doc_types: Vec<String>,
    /// Entities created by corpus resolution in this run (`Resolution::New`); only
    /// these get an `EntityName` vector, existing ones keep theirs.
    pub new_entities: HashSet<String>,
}

impl RunInfo {
    /// Records a corpus-resolution result: a `Resolution::New` key joins `new_entities`
    /// (the writer then creates the entity with its name vector), a `Link` changes
    /// nothing (the entity exists in Helix and keeps its vector).
    pub fn note_resolution(&mut self, resolution: &Resolution) {
        if let Resolution::New { entity_hk } = resolution {
            self.new_entities.insert(entity_hk.as_str().to_string());
        }
    }
}

#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Writes one document run and makes it current (SPEC.md "Write algorithm").
    async fn write_run(&self, run: &RunInfo, graph: &DocGraph, vectors: &Vectors) -> Result<(), HelixError>;
}

pub struct HelixStore {
    http: reqwest::Client,
    base_url: String,
    batch_size: usize,
}

impl HelixStore {
    pub fn new(base_url: impl Into<String>, batch_size: usize) -> Result<Self, HelixError> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().map_err(|e| HelixError::Http(e.to_string()))?;
        Ok(Self { http, base_url: base_url.into().trim_end_matches('/').to_string(), batch_size: batch_size.max(1) })
    }

    /// Calls one compiled HelixQL query.
    pub async fn query<P: Serialize + ?Sized>(&self, name: &'static str, params: &P) -> Result<Value, HelixError> {
        let resp = self
            .http
            .post(format!("{}/{name}", self.base_url))
            .json(params)
            .send()
            .await
            .map_err(|e| HelixError::Http(format!("{name}: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| HelixError::Http(format!("{name}: {e}")))?;
        if !status.is_success() {
            return Err(HelixError::Status { query: name, status: status.as_u16(), body });
        }
        // Queries that `RETURN "ok"` may answer with a bare string.
        Ok(serde_json::from_str(&body).unwrap_or(Value::String(body)))
    }

    /// Runs a batch query over `items` in chunks of `batch_size`, with `fixed` params
    /// merged into every call.
    async fn batched(&self, name: &'static str, fixed: Value, field: &str, items: Vec<Value>) -> Result<(), HelixError> {
        for chunk in items.chunks(self.batch_size) {
            let mut params = fixed.clone();
            params[field] = Value::Array(chunk.to_vec());
            self.query(name, &params).await?;
        }
        Ok(())
    }

    async fn current_run(&self, tenant: &str, document_hk: &str) -> Result<Option<String>, HelixError> {
        let v = match self.query("GetDocument", &json!({ "tenant": tenant, "document_hk": document_hk })).await {
            Ok(v) => v,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(first_object(&v["doc"]).and_then(|d| d["current_run_id"].as_str()).filter(|s| !s.is_empty()).map(String::from))
    }

    /// `Create*` that may already exist: a duplicate key means it is there.
    async fn create(&self, name: &'static str, params: &Value) -> Result<(), HelixError> {
        match self.query(name, params).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_duplicate() => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn create_entity(&self, tenant: &str, model_ref: &str, e: &EntityPlan) -> Result<(), HelixError> {
        let mut params = e.fields.clone();
        params["tenant"] = json!(tenant);
        params["uk"] = json!(format!("entity:{}", e.fields["entity_hk"].as_str().unwrap_or_default()));
        match &e.vector {
            Some(v) => {
                params["model_ref"] = json!(model_ref);
                params["vector"] = json!(v);
                self.create("CreateEntity", &params).await
            }
            None => self.create("CreateEntityWithoutVector", &params).await,
        }
    }

    /// Entities new to the corpus are created (with their vector), then all are updated
    /// in batches. A batch that hits an entity Helix does not have (resolution linked to
    /// one that is missing here) is redone entity by entity, creating what is missing.
    async fn write_entities(&self, tenant: &str, model_ref: &str, entities: &[EntityPlan]) -> Result<(), HelixError> {
        for e in entities.iter().filter(|e| e.is_new) {
            self.create_entity(tenant, model_ref, e).await?;
        }
        for chunk in entities.chunks(self.batch_size) {
            let batch: Vec<&Value> = chunk.iter().map(|e| &e.fields).collect();
            match self.query("UpdateEntities", &json!({ "tenant": tenant, "entities": batch })).await {
                Ok(_) => {}
                Err(err) if err.is_not_found() => {
                    for e in chunk {
                        self.create_entity(tenant, model_ref, e).await?;
                        self.query("UpdateEntities", &json!({ "tenant": tenant, "entities": [&e.fields] })).await?;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl GraphStore for HelixStore {
    async fn write_run(&self, run: &RunInfo, g: &DocGraph, vectors: &Vectors) -> Result<(), HelixError> {
        let plan = Plan::of(run, g, vectors)?;
        let (tenant, doc, run_id) = (run.tenant.as_str(), plan.document_hk.as_str(), run.run_id.to_string());
        let base = json!({ "tenant": tenant, "document_hk": doc, "run_id": run_id });
        let previous = self.current_run(tenant, doc).await?;

        // A partial earlier attempt of this run is removed first, so retries are safe.
        self.query("DropRun", &base).await?;
        let mut create_doc = plan.document.clone();
        create_doc["uk"] = json!(format!("document:{doc}"));
        self.create("CreateDocument", &create_doc).await?;
        self.query("UpdateDocument", &plan.document).await?;
        self.write_entities(tenant, &vectors.model_ref, &plan.entities).await?;
        self.batched("AddPages", base.clone(), "pages", plan.pages).await?;
        self.batched("AddSections", base.clone(), "sections", plan.sections).await?;
        self.batched("LinkDocumentSections", json!({ "tenant": tenant, "document_hk": doc }), "section_keys", plan.top_sections).await?;
        self.batched("LinkSubsections", json!({ "tenant": tenant }), "pairs", plan.subsections).await?;
        self.batched("AddBlocks", base.clone(), "blocks", plan.blocks).await?;
        self.batched("LinkContains", json!({ "tenant": tenant }), "pairs", plan.contains).await?;
        for (query, pairs) in plan.block_links {
            self.batched(query, json!({ "tenant": tenant }), "pairs", pairs).await?;
        }
        self.batched("AddAssertions", base.clone(), "assertions", plan.assertions).await?;
        self.batched("LinkArgs", json!({ "tenant": tenant }), "items", plan.args).await?;
        self.batched("LinkEvidence", json!({ "tenant": tenant }), "items", plan.evidence).await?;
        self.batched("LinkMentions", json!({ "tenant": tenant }), "items", plan.mentions).await?;
        self.batched("LinkRelated", json!({ "tenant": tenant, "run_id": run_id }), "items", plan.related).await?;
        let vec_params = json!({ "tenant": tenant, "run_id": run_id, "model_ref": vectors.model_ref });
        self.batched("AddBlockVectors", vec_params.clone(), "items", plan.block_vectors).await?;
        self.batched("AddSectionVectors", vec_params, "items", plan.section_vectors).await?;

        self.query("SetCurrentRun", &base).await?;
        if let Some(prev) = previous.filter(|p| *p != run_id) {
            self.query("DropRun", &json!({ "tenant": tenant, "document_hk": doc, "run_id": prev })).await?;
        }
        if plan.skipped_region_evidence > 0 {
            tracing::debug!(count = plan.skipped_region_evidence, "region evidence kept in the vault only");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct EntityPlan {
    /// `UpdateEntities` item: entity_hk, name, type_id, aliases.
    fields: Value,
    vector: Option<Vec<f64>>,
    /// Created by corpus resolution in this run.
    is_new: bool,
}

/// Query parameters derived from one `DocGraph` (pure; unit-tested).
#[derive(Debug, Default)]
struct Plan {
    document_hk: String,
    document: Value,
    entities: Vec<EntityPlan>,
    pages: Vec<Value>,
    sections: Vec<Value>,
    top_sections: Vec<Value>,
    subsections: Vec<Value>,
    blocks: Vec<Value>,
    contains: Vec<Value>,
    block_links: Vec<(&'static str, Vec<Value>)>,
    assertions: Vec<Value>,
    args: Vec<Value>,
    evidence: Vec<Value>,
    mentions: Vec<Value>,
    related: Vec<Value>,
    block_vectors: Vec<Value>,
    section_vectors: Vec<Value>,
    skipped_region_evidence: usize,
}

impl Plan {
    fn of(run: &RunInfo, g: &DocGraph, vectors: &Vectors) -> Result<Self, HelixError> {
        let run_id = run.run_id.to_string();
        let doc = g
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Document)
            .ok_or(HelixError::Response { query: "write_run", reason: "graph has no document node".into() })?;
        let kinds: HashMap<&str, NodeKind> = g.nodes.iter().map(|n| (n.key.as_str(), n.kind)).collect();
        // Pages and assertions are run-scoped in Helix but their graph keys are not.
        let page_key = |hk: &str| format!("{hk}:{run_id}");
        let assertion_key = |k: &str| format!("{k}:{run_id}");
        let vector = |key: &str| vectors.by_key.get(key).map(|v| v.iter().map(|x| *x as f64).collect::<Vec<f64>>());

        let mut p = Plan { document_hk: doc.key.clone(), ..Default::default() };
        p.document = json!({
            "tenant": run.tenant.as_str(),
            "document_hk": doc.key,
            "title": doc.props["title"].as_str().unwrap_or(""),
            "language": run.language,
            "page_count": run.page_count,
            "doc_types": run.doc_types,
        });

        let block_page: HashMap<&str, &str> =
            g.edges.iter().filter(|e| e.kind == EdgeKind::OnPage).map(|e| (e.from.as_str(), e.to.as_str())).collect();

        for n in &g.nodes {
            match n.kind {
                NodeKind::Document => {}
                NodeKind::Page => p.pages.push(json!({
                    "page_key": page_key(&n.key),
                    "page_hk": n.key,
                    "page_no": n.props["page_no"].as_i64().unwrap_or(0),
                    "doc_type": str_prop(n, "doc_type"),
                    "dag_ref": str_prop(n, "dag_ref"),
                })),
                NodeKind::Section => {
                    p.sections.push(json!({ "section_key": n.key, "title": str_prop(n, "title"), "level": n.props["level"].as_i64().unwrap_or(1) }));
                    if let Some(v) = vector(&n.key) {
                        p.section_vectors.push(json!({ "section_key": n.key, "vector": v }));
                    }
                }
                NodeKind::Block => {
                    p.blocks.push(json!({
                        "block_hk": n.key,
                        "page_key": block_page.get(n.key.as_str()).map(|hk| page_key(hk)).unwrap_or_default(),
                        "kind": str_prop(n, "kind"),
                        "text": str_prop(n, "text"),
                        "page_no": n.props["page_no"].as_i64().unwrap_or(0),
                        "bbox": bbox(&n.props["bbox"]),
                        "global_order": n.props["global_order"].as_i64().unwrap_or(0),
                        "confidence": n.props["confidence"].as_f64().unwrap_or(0.0),
                        "content": content_text(&n.props["content"]),
                    }));
                    if let Some(v) = vector(&n.key) {
                        p.block_vectors.push(json!({ "block_hk": n.key, "vector": v }));
                    }
                }
                NodeKind::Entity => p.entities.push(EntityPlan {
                    fields: json!({
                        "entity_hk": n.key,
                        "name": str_prop(n, "name"),
                        "type_id": n.props["type_id"].as_str().or(n.props["raw_type"].as_str()).unwrap_or(""),
                        "aliases": n.props["aliases"].as_array().cloned().unwrap_or_default(),
                    }),
                    vector: vector(&n.key),
                    is_new: run.new_entities.contains(&n.key),
                }),
                NodeKind::Assertion => p.assertions.push(json!({
                    "assertion_key": assertion_key(&n.key),
                    "predicate_id": n.props["predicate_id"].as_str().unwrap_or(""),
                    "predicate": str_prop(n, "predicate"),
                    "kind": str_prop(n, "kind"),
                    "qualifiers": n.props["qualifiers"].to_string(),
                    "literals": n.props["literals"].to_string(),
                    "confidence": n.props["confidence"].as_f64().unwrap_or(0.0),
                    "verified": n.props["verified"].as_bool().unwrap_or(false),
                })),
            }
        }

        let mut links: HashMap<&'static str, Vec<Value>> = HashMap::new();
        for e in &g.edges {
            let pair = || json!({ "from": e.from, "to": e.to });
            match e.kind {
                EdgeKind::HasPage | EdgeKind::OnPage => {} // created with the pages / blocks
                EdgeKind::HasSection => match kinds.get(e.from.as_str()) {
                    Some(NodeKind::Document) => p.top_sections.push(json!({ "section_key": e.to })),
                    _ => p.subsections.push(json!({ "parent": e.from, "child": e.to })),
                },
                EdgeKind::Contains => p.contains.push(json!({ "section_key": e.from, "block_hk": e.to })),
                EdgeKind::Next => links.entry("LinkNext").or_default().push(pair()),
                EdgeKind::ContinuesOn => links.entry("LinkContinuesOn").or_default().push(pair()),
                EdgeKind::TableContinues => links.entry("LinkTableContinues").or_default().push(pair()),
                EdgeKind::CaptionOf => links.entry("LinkCaptionOf").or_default().push(pair()),
                EdgeKind::RefersTo => links.entry("LinkRefersTo").or_default().push(pair()),
                EdgeKind::Mentions => p.mentions.push(json!({
                    "block_hk": e.from,
                    "entity_hk": e.to,
                    "char_start": e.props["char_start"].as_i64().unwrap_or(0),
                    "char_end": e.props["char_end"].as_i64().unwrap_or(0),
                    "surface": e.props["surface"].as_str().unwrap_or(""),
                    "confidence": e.props["confidence"].as_f64().unwrap_or(0.0),
                })),
                EdgeKind::Arg => p.args.push(json!({ "assertion_key": assertion_key(&e.from), "entity_hk": e.to, "role": e.props["role"].as_str().unwrap_or("") })),
                EdgeKind::Evidence => {
                    if kinds.get(e.to.as_str()) == Some(&NodeKind::Block) {
                        p.evidence.push(json!({
                            "assertion_key": assertion_key(&e.from),
                            "block_hk": e.to,
                            "char_start": e.props["char_start"].as_i64().unwrap_or(0),
                            "char_end": e.props["char_end"].as_i64().unwrap_or(0),
                        }));
                    } else {
                        p.skipped_region_evidence += 1; // visual (page + bbox) evidence: vault only
                    }
                }
                EdgeKind::Related => p.related.push(json!({
                    "from": e.from,
                    "to": e.to,
                    "predicate_id": e.props["predicate"].as_str().unwrap_or(""),
                    "assertion_key": e.props["assertion"].as_str().map(assertion_key).unwrap_or_default(),
                    "confidence": e.props["confidence"].as_f64().unwrap_or(0.0),
                })),
                EdgeKind::Cites | EdgeKind::SameAs => skip_edge(e),
            }
        }
        let mut block_links: Vec<_> = links.into_iter().collect();
        block_links.sort_by_key(|(q, _)| *q);
        p.block_links = block_links;
        Ok(p)
    }
}

fn skip_edge(e: &GEdge) {
    tracing::debug!(kind = ?e.kind, "edge kind not stored in Helix yet");
}

fn str_prop(n: &GNode, field: &str) -> String {
    match &n.props[field] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string().trim_matches('"').to_string(),
    }
}

fn bbox(v: &Value) -> Vec<f64> {
    ["x0", "y0", "x1", "y1"].iter().map(|k| v[k].as_f64().unwrap_or(0.0)).collect()
}

/// Non-text block content as one string: table HTML, formula LaTeX, figure/chart
/// description (plus chart data table). Empty for text blocks.
fn content_text(c: &Value) -> String {
    let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
    match c["type"].as_str() {
        Some("table") => s("html"),
        Some("formula") => s("latex"),
        Some("figure") => s("description"),
        Some("chart") => [s("description"), s("data_html")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Helix returns each `RETURN` variable as a value or an array of values.
fn first_object(v: &Value) -> Option<&Value> {
    match v {
        Value::Array(a) => a.first(),
        Value::Object(_) => Some(v),
        _ => None,
    }
}

// ─────────────────────── Corpus entity resolution ───────────────────────

/// LLM judge for candidates in the uncertain score band.
#[derive(Clone)]
pub struct Judge {
    pub client: Arc<OpenAiClient>,
    /// Model id in the registry (a `generic_json` text LLM, task `entity_judge`).
    pub model: String,
}

/// `vs_extract::resolve::CorpusResolver` against HelixDB (SPEC.md "Corpus entity
/// resolution"): embed `name (type)` + context, vector-search `entity_name` within the
/// tenant, score candidates of a compatible type by cosine and name similarity, then
/// link / ask the judge / create a new entity.
pub struct HelixResolver {
    pub store: Arc<HelixStore>,
    pub embedder: Arc<dyn Embedder>,
    pub judge: Option<Judge>,
    /// Score at or above which a candidate is linked without asking.
    pub link_threshold: f32,
    /// `(low, high)`: scores in `low..high` go to the judge (none configured → new entity).
    pub judge_band: (f32, f32),
    /// Candidates kept after the type filter.
    pub k: usize,
}

/// One search hit joined with its entity.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub entity_hk: String,
    pub name: String,
    pub type_id: String,
    pub aliases: Vec<String>,
    pub cosine: f32,
}

/// What the thresholds say about a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Link,
    Judge,
    New,
}

pub const DEFAULT_LINK_THRESHOLD: f32 = 0.85;
pub const DEFAULT_JUDGE_BAND: (f32, f32) = (0.7, 0.85);
pub const DEFAULT_K: usize = 10;

impl HelixResolver {
    /// Resolver with the spec defaults (link ≥ 0.85, judge band 0.7..0.85, k = 10) and no judge.
    pub fn new(store: Arc<HelixStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder, judge: None, link_threshold: DEFAULT_LINK_THRESHOLD, judge_band: DEFAULT_JUDGE_BAND, k: DEFAULT_K }
    }

    pub fn with_judge(mut self, client: Arc<OpenAiClient>, model: impl Into<String>) -> Self {
        self.judge = Some(Judge { client, model: model.into() });
        self
    }

    pub fn with_thresholds(mut self, link_threshold: f32, judge_band: (f32, f32)) -> Self {
        self.link_threshold = link_threshold;
        self.judge_band = judge_band;
        self
    }

    pub fn with_k(mut self, k: usize) -> Self {
        self.k = k.max(1);
        self
    }

    /// Text embedded for corpus resolution: `name (type)` plus the context sentence.
    /// Without context this equals the `entity_name` embed target text of `vs-graph`.
    pub fn embed_text(name: &str, type_id: &str, context: &str) -> String {
        let head = format!("{name} ({type_id})");
        match context.trim() {
            "" => head,
            ctx => format!("{head}\n{ctx}"),
        }
    }

    /// `H(tenant | type_id | normalised name)`: the key of an entity new to the corpus.
    /// Deterministic, so the same name and type in two documents meet in Helix even
    /// when the search missed (and `write_entities` treats the duplicate as "exists").
    pub fn new_entity_key(tenant: &TenantId, type_id: &str, name: &str) -> HashKey {
        HashKey::entity(tenant, &format!("{type_id}|{}", normalise_name(name)))
    }

    /// `0.6 · cosine + 0.4 · max Jaro-Winkler(normalised name, candidate name / aliases)`.
    pub fn score(name: &str, aliases: &[String], candidate: &Candidate) -> f32 {
        let ours: Vec<String> = std::iter::once(name).chain(aliases.iter().map(String::as_str)).map(normalise_name).collect();
        let theirs: Vec<String> = std::iter::once(candidate.name.as_str()).chain(candidate.aliases.iter().map(String::as_str)).map(normalise_name).collect();
        let name_sim = ours
            .iter()
            .flat_map(|a| theirs.iter().map(move |b| jaro_winkler(a, b)))
            .fold(0.0f32, f32::max);
        0.6 * candidate.cosine.clamp(-1.0, 1.0) + 0.4 * name_sim
    }

    pub fn verdict(&self, score: f32) -> Verdict {
        if score >= self.link_threshold {
            Verdict::Link
        } else if score >= self.judge_band.0 && score < self.judge_band.1 {
            Verdict::Judge
        } else {
            Verdict::New
        }
    }

    /// Same type id, or either side has none (unnormalised type).
    pub fn compatible_type(ours: Option<&str>, theirs: &str) -> bool {
        match ours {
            None | Some("") => true,
            Some(t) => theirs.is_empty() || t == theirs,
        }
    }

    /// Joins a `SearchEntityNames` response (`vectors` + `entities`) into candidates with
    /// their cosine similarity to `query`. Vectors without `data` get `None` and are
    /// re-embedded by `resolve`.
    pub fn candidates(response: &Value, query: &[f32]) -> Vec<(Candidate, Option<f32>)> {
        let cos_by_hk: HashMap<String, f32> = as_array(&response["vectors"])
            .iter()
            .filter_map(|v| {
                let hk = v["entity_hk"].as_str()?;
                let data: Vec<f32> = v["data"].as_array()?.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
                (data.len() == query.len() && !data.is_empty()).then(|| (hk.to_string(), cosine(query, &data)))
            })
            .collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for e in as_array(&response["entities"]) {
            let Some(hk) = e["entity_hk"].as_str() else { continue };
            if !seen.insert(hk.to_string()) {
                continue; // an entity with several vectors appears once per vector
            }
            let cos = cos_by_hk.get(hk).copied();
            out.push((
                Candidate {
                    entity_hk: hk.to_string(),
                    name: e["name"].as_str().unwrap_or("").to_string(),
                    type_id: e["type_id"].as_str().unwrap_or("").to_string(),
                    aliases: as_array(&e["aliases"]).iter().filter_map(|a| a.as_str().map(String::from)).collect(),
                    cosine: cos.unwrap_or(0.0),
                },
                cos,
            ));
        }
        out
    }

    /// The decision for one entity given the search response (everything after the
    /// Helix call; unit-tested with a fake embedder and a canned response).
    pub async fn decide(&self, tenant: &TenantId, entity: &DocEntity, context: &str, query: &[f32], response: &Value) -> Result<Resolution, ExtractError> {
        let type_id = entity.type_id.as_deref().unwrap_or(&entity.raw_type);
        let mut found = Self::candidates(response, query);
        found.retain(|(c, _)| Self::compatible_type(entity.type_id.as_deref(), &c.type_id));

        // Vectors Helix did not hand back: embed `name (type)` of those candidates.
        let missing: Vec<usize> = found.iter().enumerate().filter(|(_, (_, cos))| cos.is_none()).map(|(i, _)| i).collect();
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|&i| Self::embed_text(&found[i].0.name, &found[i].0.type_id, "")).collect();
            let vecs = self.embedder.embed_texts(&texts).await.map_err(|e| ExtractError::Model(e.to_string()))?;
            for (&i, v) in missing.iter().zip(&vecs) {
                found[i].0.cosine = cosine(query, v);
            }
        }

        let mut scored: Vec<(f32, Candidate)> = found.into_iter().map(|(c, _)| (Self::score(&entity.name, &entity.aliases, &c), c)).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(self.k);

        for (score, cand) in &scored {
            match self.verdict(*score) {
                Verdict::Link => return Ok(Resolution::Link { entity_hk: stored_key(&cand.entity_hk)?, score: *score }),
                Verdict::Judge => {
                    let Some(judge) = &self.judge else { break };
                    if judge.same_entity(entity, type_id, context, cand).await? {
                        return Ok(Resolution::Link { entity_hk: stored_key(&cand.entity_hk)?, score: *score });
                    }
                }
                Verdict::New => break, // sorted: nothing below can do better
            }
        }
        Ok(Resolution::New { entity_hk: Self::new_entity_key(tenant, type_id, &entity.name) })
    }
}

impl Judge {
    /// Asks the LLM whether `entity` (with its context sentence) and `cand` are the same
    /// real-world entity; constrained to `{"same": bool}`.
    async fn same_entity(&self, entity: &DocEntity, type_id: &str, context: &str, cand: &Candidate) -> Result<bool, ExtractError> {
        let prompt = format!(
            "Entity A (from the document being processed)\n  name: {}\n  type: {}\n  aliases: {}\n  context: {}\n\n\
             Entity B (already in the corpus)\n  name: {}\n  type: {}\n  aliases: {}\n\n\
             Are A and B the same real-world entity (same method, dataset, organisation, …), not merely related or similar?",
            entity.name,
            type_id,
            join_or_none(&entity.aliases),
            if context.trim().is_empty() { "(none)" } else { context.trim() },
            cand.name,
            if cand.type_id.is_empty() { "(unknown)" } else { &cand.type_id },
            join_or_none(&cand.aliases),
        );
        let req = ChatRequest {
            system: Some("You decide whether two entity records refer to the same real-world entity. Answer only with the JSON object.".into()),
            prompt,
            images: vec![],
            max_tokens: 32,
            json_schema: Some(json!({
                "type": "object",
                "properties": { "same": { "type": "boolean" } },
                "required": ["same"],
                "additionalProperties": false
            })),
        };
        let resp = self.client.chat(&self.model, &req).await.map_err(|e| ExtractError::Model(e.to_string()))?;
        let v: Value = serde_json::from_str(resp.text.trim()).map_err(|e| ExtractError::Schema(format!("entity judge: {e}: {}", resp.text)))?;
        v["same"].as_bool().ok_or_else(|| ExtractError::Schema(format!("entity judge: missing `same` in {}", resp.text)))
    }
}

#[async_trait]
impl CorpusResolver for HelixResolver {
    async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, context: &str) -> Result<Resolution, ExtractError> {
        let type_id = entity.type_id.as_deref().unwrap_or(&entity.raw_type);
        let text = Self::embed_text(&entity.name, type_id, context);
        let mut vecs = self.embedder.embed_texts(&[text]).await.map_err(|e| ExtractError::Model(e.to_string()))?;
        let query = vecs.pop().ok_or_else(|| ExtractError::Model("embedder returned no vector".into()))?;
        // Tenant and type are post-filters, so ask for more than k.
        let params = json!({ "tenant": tenant.as_str(), "vector": query.iter().map(|x| *x as f64).collect::<Vec<_>>(), "k": (self.k * 4) as i64 });
        let response = match self.store.query("SearchEntityNames", &params).await {
            Ok(v) => v,
            Err(e) if e.is_not_found() => Value::Null, // empty index
            Err(e) => return Err(ExtractError::Model(format!("helix: {e}"))),
        };
        self.decide(tenant, entity, context, &query, &response).await
    }
}

/// A hash key as Helix stored it (`HashKey` is serde-transparent over its hex string).
fn stored_key(hex: &str) -> Result<HashKey, ExtractError> {
    serde_json::from_value(Value::String(hex.to_string())).map_err(|e| ExtractError::Schema(format!("entity_hk {hex:?}: {e}")))
}

fn join_or_none(xs: &[String]) -> String {
    if xs.is_empty() { "(none)".to_string() } else { xs.join(", ") }
}

fn as_array(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        other => vec![other.clone()],
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Jaro similarity over chars, in `0..=1`.
pub fn jaro(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let (mut a_hit, mut b_hit) = (vec![false; a.len()], vec![false; b.len()]);
    let mut matches = 0usize;
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_hit[j] && b[j] == *ca {
                a_hit[i] = true;
                b_hit[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut transpositions = 0usize;
    let mut j = 0;
    for (i, &hit) in a_hit.iter().enumerate() {
        if !hit {
            continue;
        }
        while !b_hit[j] {
            j += 1;
        }
        if a[i] != b[j] {
            transpositions += 1;
        }
        j += 1;
    }
    let m = matches as f32;
    (m / a.len() as f32 + m / b.len() as f32 + (m - (transpositions / 2) as f32) / m) / 3.0
}

/// Jaro-Winkler with the standard prefix scale 0.1 over at most 4 leading chars.
pub fn jaro_winkler(a: &str, b: &str) -> f32 {
    let j = jaro(a, b);
    let prefix = a.chars().zip(b.chars()).take(4).take_while(|(x, y)| x == y).count() as f32;
    j + prefix * 0.1 * (1.0 - j)
}

pub fn normalise_name(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_normalisation() {
        assert_eq!(normalise_name("  Vision   Transformer "), "vision transformer");
    }

    #[test]
    fn jaro_winkler_reference_values() {
        assert_eq!(jaro_winkler("", ""), 1.0);
        assert_eq!(jaro_winkler("abc", ""), 0.0);
        assert_eq!(jaro_winkler("bert", "bert"), 1.0);
        assert!((jaro("martha", "marhta") - 0.9444).abs() < 1e-3);
        assert!((jaro_winkler("martha", "marhta") - 0.9611).abs() < 1e-3);
        assert!((jaro_winkler("dwayne", "duane") - 0.84).abs() < 1e-3);
        assert!(jaro_winkler("bert", "gpt-3") < 0.5);
    }

    fn cand(name: &str, type_id: &str, aliases: &[&str], cosine: f32) -> Candidate {
        Candidate { entity_hk: format!("hk-{name}"), name: name.into(), type_id: type_id.into(), aliases: aliases.iter().map(|s| s.to_string()).collect(), cosine }
    }

    #[test]
    fn score_mixes_cosine_and_best_name_similarity() {
        let c = cand("Bidirectional Encoder Representations from Transformers", "method", &["BERT"], 0.9);
        let s = HelixResolver::score("BERT", &[], &c);
        assert!((s - (0.6 * 0.9 + 0.4 * 1.0)).abs() < 1e-5, "alias match counts fully: {s}");
        let far = cand("ImageNet", "dataset", &[], 0.0);
        assert!(HelixResolver::score("BERT", &[], &far) < 0.2);
        // Our own aliases count too, and whitespace/case do not matter.
        let c2 = cand("vision transformer", "method", &[], 0.5);
        assert!((HelixResolver::score("ViT", &["Vision  Transformer".into()], &c2) - (0.3 + 0.4)).abs() < 1e-5);
    }

    #[test]
    fn type_compatibility() {
        assert!(HelixResolver::compatible_type(Some("method"), "method"));
        assert!(HelixResolver::compatible_type(Some("method"), ""));
        assert!(HelixResolver::compatible_type(None, "dataset"));
        assert!(!HelixResolver::compatible_type(Some("method"), "dataset"));
    }

    struct FakeEmbedder;
    #[async_trait]
    impl Embedder for FakeEmbedder {
        fn model_ref(&self) -> String {
            "fake@1".into()
        }
        fn dimensions(&self) -> usize {
            2
        }
        async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, vs_models::ModelError> {
            // Anything mentioning "bert" points along x, everything else along y.
            Ok(texts.iter().map(|t| if t.to_lowercase().contains("bert") { vec![1.0, 0.0] } else { vec![0.0, 1.0] }).collect())
        }
    }

    fn resolver() -> HelixResolver {
        HelixResolver::new(Arc::new(HelixStore::new("http://127.0.0.1:1", 10).unwrap()), Arc::new(FakeEmbedder))
    }

    fn entity(name: &str, type_id: Option<&str>) -> DocEntity {
        DocEntity { local_id: "e1".into(), name: name.into(), raw_type: "Method".into(), type_id: type_id.map(String::from), aliases: vec![], mentions: vec![], entity_hk: None }
    }

    fn response(entities: Vec<(&str, &str, Vec<f32>)>) -> Value {
        json!({
            "vectors": entities.iter().map(|(hk, _, v)| json!({ "entity_hk": hk, "model_ref": "fake@1", "data": v })).collect::<Vec<_>>(),
            "entities": entities.iter().map(|(hk, name, _)| json!({ "entity_hk": hk, "name": name, "type_id": "method", "aliases": [] })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn verdict_thresholds() {
        let r = resolver();
        assert_eq!(r.verdict(0.85), Verdict::Link);
        assert_eq!(r.verdict(0.9), Verdict::Link);
        assert_eq!(r.verdict(0.8499), Verdict::Judge);
        assert_eq!(r.verdict(0.7), Verdict::Judge);
        assert_eq!(r.verdict(0.69), Verdict::New);
        let r = r.with_thresholds(0.95, (0.5, 0.95));
        assert_eq!((r.verdict(0.9), r.verdict(0.4)), (Verdict::Judge, Verdict::New));
    }

    #[tokio::test]
    async fn decides_link_judge_and_new() {
        let r = resolver();
        let tenant = TenantId("t".into());
        let bert = entity("BERT", Some("method"));
        let hex = "ab".repeat(32);

        // Exact name, aligned vector → link to the stored key.
        let resp = response(vec![(&hex, "BERT", vec![1.0, 0.0])]);
        let res = r.decide(&tenant, &bert, "", &[1.0, 0.0], &resp).await.unwrap();
        assert!(matches!(&res, Resolution::Link { entity_hk, score } if entity_hk.as_str() == hex && (*score - 1.0).abs() < 1e-5), "{res:?}");

        // Score in the judge band without a judge → new entity with the deterministic key.
        let resp = response(vec![(&hex, "RoBERTa", vec![0.8, 0.6])]);
        let c = cand("RoBERTa", "method", &[], 0.8);
        let s = HelixResolver::score("BERT", &[], &c);
        assert_eq!(r.verdict(s), Verdict::Judge, "score {s}");
        let res = r.decide(&tenant, &bert, "", &[1.0, 0.0], &resp).await.unwrap();
        assert_eq!(res, Resolution::New { entity_hk: HashKey::entity(&tenant, "method|bert") });

        // Orthogonal vector and different name → new.
        let resp = response(vec![(&hex, "ImageNet", vec![0.0, 1.0])]);
        let res = r.decide(&tenant, &bert, "", &[1.0, 0.0], &resp).await.unwrap();
        assert!(matches!(res, Resolution::New { .. }));

        // Incompatible type is filtered even with a perfect score.
        let resp = json!({
            "vectors": [{ "entity_hk": hex, "data": [1.0, 0.0] }],
            "entities": [{ "entity_hk": hex, "name": "BERT", "type_id": "dataset", "aliases": [] }],
        });
        let res = r.decide(&tenant, &bert, "", &[1.0, 0.0], &resp).await.unwrap();
        assert!(matches!(res, Resolution::New { .. }));

        // Candidate without vector data: re-embedded through the embedder.
        let resp = json!({ "vectors": [], "entities": [{ "entity_hk": hex, "name": "BERT", "type_id": "", "aliases": [] }] });
        let res = r.decide(&tenant, &bert, "", &[1.0, 0.0], &resp).await.unwrap();
        assert!(matches!(res, Resolution::Link { .. }), "{res:?}");

        // Empty / absent response and no type id → new key uses the raw type.
        let raw = entity("BERT", None);
        let res = r.decide(&tenant, &raw, "", &[1.0, 0.0], &Value::Null).await.unwrap();
        assert_eq!(res, Resolution::New { entity_hk: HashKey::entity(&tenant, "Method|bert") });
    }

    #[test]
    fn run_info_notes_new_entities_only() {
        let mut run = RunInfo { tenant: TenantId("t".into()), run_id: RunId(uuid::Uuid::nil()), language: "en".into(), page_count: 0, doc_types: vec![], new_entities: HashSet::new() };
        let new = HashKey::entity(&run.tenant, "method|bert");
        run.note_resolution(&Resolution::New { entity_hk: new.clone() });
        run.note_resolution(&Resolution::Link { entity_hk: HashKey::entity(&run.tenant, "x"), score: 0.9 });
        assert_eq!(run.new_entities, HashSet::from([new.as_str().to_string()]));
    }

    #[test]
    fn embed_text_matches_graph_target_without_context() {
        assert_eq!(HelixResolver::embed_text("BERT", "method", ""), "BERT (method)");
        assert_eq!(HelixResolver::embed_text("BERT", "method", " fine-tuned on GLUE "), "BERT (method)\nfine-tuned on GLUE");
    }

    fn node(kind: NodeKind, key: &str, props: Value) -> GNode {
        GNode { kind, key: key.into(), props }
    }

    fn edge(kind: EdgeKind, from: &str, to: &str, props: Value) -> GEdge {
        GEdge { kind, from: from.into(), to: to.into(), props }
    }

    #[test]
    fn plan_maps_graph_to_query_params() {
        let g = DocGraph {
            nodes: vec![
                node(NodeKind::Document, "d", json!({ "title": "T" })),
                node(NodeKind::Page, "p1", json!({ "page_no": 1 })),
                node(NodeKind::Section, "sec:h", json!({ "title": "Intro", "level": 1 })),
                node(NodeKind::Section, "sec:h2", json!({ "title": "Sub", "level": 2 })),
                node(NodeKind::Block, "b1", json!({ "kind": "paragraph", "text": "x", "page_no": 1, "global_order": 0, "confidence": 0.9,
                    "bbox": { "x0": 0.1, "y0": 0.2, "x1": 0.3, "y1": 0.4 }, "content": { "type": "text", "text": "x" } })),
                node(NodeKind::Block, "b2", json!({ "kind": "table", "text": "t", "page_no": 1, "global_order": 1, "confidence": 0.8,
                    "bbox": { "x0": 0.0, "y0": 0.5, "x1": 1.0, "y1": 0.9 }, "content": { "type": "table", "html": "<table/>" } })),
                node(NodeKind::Entity, "e1", json!({ "name": "BERT", "type_id": "method", "aliases": ["bert"] })),
                node(NodeKind::Entity, "e2", json!({ "name": "GLUE", "type_id": null, "raw_type": "dataset", "aliases": [] })),
                node(NodeKind::Assertion, "asr:d:a1", json!({ "predicate": "evaluated on", "predicate_id": "uses", "kind": "binary",
                    "qualifiers": { "negated": false }, "literals": [], "confidence": 0.7, "verified": true })),
            ],
            edges: vec![
                edge(EdgeKind::HasPage, "d", "p1", Value::Null),
                edge(EdgeKind::OnPage, "b1", "p1", Value::Null),
                edge(EdgeKind::OnPage, "b2", "p1", Value::Null),
                edge(EdgeKind::HasSection, "d", "sec:h", Value::Null),
                edge(EdgeKind::HasSection, "sec:h", "sec:h2", Value::Null),
                edge(EdgeKind::Contains, "sec:h", "b1", Value::Null),
                edge(EdgeKind::Next, "b1", "b2", Value::Null),
                edge(EdgeKind::Arg, "asr:d:a1", "e1", json!({ "role": "subject" })),
                edge(EdgeKind::Evidence, "asr:d:a1", "b1", json!({ "char_start": 0, "char_end": 1 })),
                edge(EdgeKind::Evidence, "asr:d:a1", "p1", json!({ "bbox": {} })),
                edge(EdgeKind::Related, "e1", "e2", json!({ "predicate": "uses", "assertion": "asr:d:a1", "confidence": 0.7 })),
            ],
            embed_targets: vec![],
        };
        let run = RunInfo {
            tenant: TenantId("t".into()),
            run_id: RunId(uuid::Uuid::nil()),
            language: "en".into(),
            page_count: 1,
            doc_types: vec!["scientific_paper".into()],
            new_entities: HashSet::from(["e2".to_string()]),
        };
        let vectors = Vectors {
            model_ref: "m@1".into(),
            dimensions: 2,
            by_key: HashMap::from([("b1".into(), vec![0.5, 0.25]), ("e1".into(), vec![1.0, 0.0]), ("e2".into(), vec![0.0, 1.0])]),
        };
        let p = Plan::of(&run, &g, &vectors).unwrap();
        let rid = uuid::Uuid::nil().to_string();

        assert_eq!(p.document["title"], "T");
        assert_eq!(p.pages[0]["page_key"], format!("p1:{rid}"));
        assert_eq!(p.blocks[0]["page_key"], format!("p1:{rid}"));
        assert_eq!(p.blocks[0]["bbox"], json!([0.1, 0.2, 0.3, 0.4]));
        assert_eq!(p.blocks[0]["content"], "");
        assert_eq!(p.blocks[1]["content"], "<table/>");
        assert_eq!(p.top_sections, vec![json!({ "section_key": "sec:h" })]);
        assert_eq!(p.subsections, vec![json!({ "parent": "sec:h", "child": "sec:h2" })]);
        assert_eq!(p.block_links, vec![("LinkNext", vec![json!({ "from": "b1", "to": "b2" })])]);
        assert_eq!(p.entities[1].fields["type_id"], "dataset"); // falls back to raw type
        assert_eq!((p.entities[0].is_new, p.entities[1].is_new), (false, true));
        assert_eq!(p.entities[1].vector, Some(vec![0.0, 1.0]));
        assert_eq!(p.block_vectors.len(), 1);
        assert_eq!(p.assertions[0]["assertion_key"], format!("asr:d:a1:{rid}"));
        assert_eq!(p.assertions[0]["qualifiers"], r#"{"negated":false}"#);
        assert_eq!(p.args[0]["assertion_key"], format!("asr:d:a1:{rid}"));
        assert_eq!(p.evidence.len(), 1);
        assert_eq!(p.skipped_region_evidence, 1);
        assert_eq!(p.related[0]["assertion_key"], format!("asr:d:a1:{rid}"));
    }
}
