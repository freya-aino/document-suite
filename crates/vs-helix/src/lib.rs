//! HelixDB writer. See `SPEC.md`.
//!
//! HelixDB is only called through the compiled HelixQL queries in `helix/db/queries.hx`
//! (`POST {base_url}/{QueryName}` with the query's parameters as a JSON object).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use vs_core::{HashKey, RunId, TenantId};
use vs_extract::resolve::{CorpusResolver, Resolution};
use vs_extract::{DocEntity, ExtractError};
use vs_graph::{DocGraph, EdgeKind, GEdge, GNode, NodeKind};

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

pub struct HelixResolver {
    pub store: std::sync::Arc<HelixStore>,
    pub link_threshold: f32,
    pub judge_band: (f32, f32),
}

#[async_trait]
impl CorpusResolver for HelixResolver {
    async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, _context: &str) -> Result<Resolution, ExtractError> {
        // v1 placeholder: no corpus lookup yet → deterministic new key (same name+type → same entity).
        let type_id = entity.type_id.as_deref().unwrap_or(&entity.raw_type);
        let key = format!("{}|{}", type_id, normalise_name(&entity.name));
        Ok(Resolution::New { entity_hk: HashKey::entity(tenant, &key) })
    }
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
