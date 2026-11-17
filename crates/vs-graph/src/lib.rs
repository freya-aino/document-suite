//! Document graph model and builder. See `README.md`.

pub mod page;
mod store;
pub use store::{GraphStore, RunInfo, StoreError, Vectors};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use vs_core::{BlockKind, Evidence, HashKey};
use vs_extract::{ArgValue, AssertionKind, ExtractionOutput};
use vs_stitch::{LinkKind, StitchedDocument};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Document,
    Page,
    Section,
    Block,
    Entity,
    Assertion,
    /// One cell of a table, so a single number can be cited rather than "that table".
    Cell,
    /// A fragment of text found inside a figure or chart.
    ImageText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    HasPage,
    HasSection,
    Contains,
    OnPage,
    Next,
    ContinuesOn,
    TableContinues,
    CaptionOf,
    RefersTo,
    Mentions,
    Arg,
    Evidence,
    Related,
    Cites,
    /// Block → block, same page: the first geometrically encloses the second.
    EnclosesRegion,
    /// Block → block, same page: the first is the value of the second, a form label.
    ValueOf,
    /// Table → cell.
    HasCell,
    /// Figure/chart block → text found inside the image.
    TextInImage,
    /// In-image text → the block or entity elsewhere in the document that it matches.
    MatchesText,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GNode {
    pub kind: NodeKind,
    pub key: String,
    pub props: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GEdge {
    pub kind: EdgeKind,
    pub from: String,
    pub to: String,
    pub props: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedTarget {
    pub node_key: String,
    pub kind: NodeKind,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocGraph {
    pub nodes: Vec<GNode>,
    pub edges: Vec<GEdge>,
    pub embed_targets: Vec<EmbedTarget>,
}

#[derive(Debug, PartialEq)]
pub enum GraphError {
    DuplicateNode(String),
    DanglingEdge { kind: EdgeKind, from: String, to: String },
    AssertionWithoutEvidence(String),
    /// An assertion with no arguments and no literals asserts nothing about anything.
    AssertionWithoutArguments(String),
    /// An edge between the wrong kinds of node, e.g. `on_page` pointing at an entity.
    WrongEndpointKind { kind: EdgeKind, from: NodeKind, to: NodeKind },
}

/// The node kinds each edge may join, as `(from, to)`. `None` means unconstrained.
///
/// Without this, nothing stopped `Contains` running page → page or `OnPage` pointing at
/// an entity: the dangling check only asked whether the keys existed, and keys are
/// unique per *kind*, so an edge could even be satisfied by a same-key node of another
/// kind entirely.
fn endpoints(kind: EdgeKind) -> Option<(NodeKind, NodeKind)> {
    use EdgeKind::*;
    use NodeKind::*;
    Some(match kind {
        HasPage => (Document, Page),
        OnPage => (Block, Page),
        Contains => (Section, Block),
        Next | ContinuesOn | TableContinues | CaptionOf | RefersTo | EnclosesRegion | ValueOf => (Block, Block),
        Mentions => (Block, Entity),
        Arg => (Assertion, Entity),
        // Entity → entity, but both sides may be a doc-local or a canonical key, so the
        // kind is the same on both ends and the constraint adds nothing.
        Related => (Entity, Entity),
        Cites => (Block, Entity),
        HasCell => (Block, Cell),
        TextInImage => (Block, ImageText),
        // `evidence` points at a block for a span and a page for a region, and
        // `matches_text` at either a block or an entity, so neither has one shape.
        Evidence | MatchesText => return None,
        // `has_section` is document → section at the top level and section → section
        // when nested.
        HasSection => return None,
    })
}

impl DocGraph {

    fn node(&mut self, kind: NodeKind, key: impl Into<String>, props: Value) {
        self.nodes.push(GNode { kind, key: key.into(), props });
    }

    fn edge(&mut self, kind: EdgeKind, from: impl Into<String>, to: impl Into<String>, props: Value) {
        self.edges.push(GEdge { kind, from: from.into(), to: to.into(), props });
    }

    /// For the `page` module, which adds the nodes and edges inside a block.
    pub(crate) fn push_node(&mut self, kind: NodeKind, key: &str, props: Value) {
        self.node(kind, key, props);
    }

    pub(crate) fn push_edge(&mut self, kind: EdgeKind, from: &str, to: &str, props: Value) {
        self.edge(kind, from, to, props);
    }

    pub fn validate(&self) -> Result<(), GraphError> {
        let mut by_key: HashMap<&str, NodeKind> = HashMap::new();
        let mut seen = HashSet::new();
        for n in &self.nodes {
            if !seen.insert((n.kind, n.key.as_str())) {
                return Err(GraphError::DuplicateNode(n.key.clone()));
            }
            by_key.insert(n.key.as_str(), n.kind);
        }
        for e in &self.edges {
            // Resolve both ends to a *kind*, not just to "some node exists with this
            // key". Keys are unique per kind, so the old existence check could be
            // satisfied by a node of a completely different kind.
            let (Some(from), Some(to)) = (by_key.get(e.from.as_str()), by_key.get(e.to.as_str())) else {
                return Err(GraphError::DanglingEdge { kind: e.kind, from: e.from.clone(), to: e.to.clone() });
            };
            if let Some((want_from, want_to)) = endpoints(e.kind) {
                if *from != want_from || *to != want_to {
                    return Err(GraphError::WrongEndpointKind { kind: e.kind, from: *from, to: *to });
                }
            }
        }
        for a in self.nodes.iter().filter(|n| n.kind == NodeKind::Assertion) {
            if !self.edges.iter().any(|e| e.kind == EdgeKind::Evidence && e.from == a.key) {
                return Err(GraphError::AssertionWithoutEvidence(a.key.clone()));
            }
            // An assertion needs something to be about: at least one entity argument or
            // one literal. the spec claimed this from the start; nothing checked it,
            // so an extraction that produced a bare predicate passed validation.
            let has_arg = self.edges.iter().any(|e| e.kind == EdgeKind::Arg && e.from == a.key);
            let has_literal = a.props.get("literals").and_then(|l| l.as_array()).is_some_and(|l| !l.is_empty());
            if !has_arg && !has_literal {
                return Err(GraphError::AssertionWithoutArguments(a.key.clone()));
            }
        }
        Ok(())
    }
}

/// Builds the graph from stitched structure and extraction output.
pub fn build(doc: &StitchedDocument, ext: &ExtractionOutput, title: Option<&str>) -> DocGraph {
    let mut g = DocGraph::default();
    let dk = doc.document_hk.to_string();
    g.node(NodeKind::Document, &dk, json!({ "title": title, "run_id": doc.run_id, "missing_pages": doc.missing_pages }));

    // Pages
    let mut pages: Vec<(u32, &HashKey)> = doc.blocks.iter().map(|b| (b.page_no, &b.page_hk)).collect();
    pages.sort_by_key(|p| p.0);
    pages.dedup_by_key(|p| p.0);
    for (no, hk) in &pages {
        g.node(NodeKind::Page, hk.as_str(), json!({ "page_no": no }));
        g.edge(EdgeKind::HasPage, &dk, hk.as_str(), Value::Null);
    }

    // Blocks
    let mut prev_body: Option<&str> = None;
    let mut body_text: Vec<(String, String)> = Vec::new();
    let mut image_blocks: Vec<(String, vs_core::BlockContent)> = Vec::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        let bk = b.block.block_hk.as_str();
        g.node(NodeKind::Block, bk, json!({
            "kind": b.block.kind, "text": b.block.text, "bbox": b.block.bbox, "page_no": b.page_no,
            "global_order": b.global_order, "confidence": b.block.confidence, "content": b.block.content,
            "furniture": !doc.is_body(i),
        }));
        g.edge(EdgeKind::OnPage, bk, b.page_hk.as_str(), Value::Null);
        // A table becomes addressable cell by cell, so one number can be cited rather
        // than the whole table.
        page::add_table_cells(&mut g, bk, &b.block.content);
        if doc.is_body(i) {
            body_text.push((bk.to_string(), b.block.text.clone()));
            if page::is_image_kind(b.block.kind) {
                image_blocks.push((bk.to_string(), b.block.content.clone()));
            }
        }
        if doc.is_body(i) {
            if let Some(p) = prev_body {
                g.edge(EdgeKind::Next, p, bk, Value::Null);
            }
            prev_body = Some(bk);
            // A figure's searchable text is normally its description plus in-image text,
            // but a `figure.describe` that returned only a description leaves `text`
            // empty — and the spec promises figures are embeddable. Fall back to it.
            let text = match b.block.text.trim().is_empty() {
                false => b.block.text.clone(),
                true => match &b.block.content {
                    vs_core::BlockContent::Figure { description: Some(d), .. }
                    | vs_core::BlockContent::Chart { description: Some(d), .. } => d.clone(),
                    _ => String::new(),
                },
            };
            let embeddable = !text.trim().is_empty()
                && !matches!(b.block.kind, BlockKind::PageNumber | BlockKind::Header | BlockKind::Footer);
            if embeddable {
                g.embed_targets.push(EmbedTarget { node_key: bk.to_string(), kind: NodeKind::Block, text });
            }
        }
    }

    // Sections
    for s in &doc.sections {
        let sk = s.heading.as_str();
        g.node(NodeKind::Section, format!("sec:{sk}"), json!({ "title": s.title, "level": s.level, "heading_block": sk }));
        match s.parent {
            Some(p) => g.edge(EdgeKind::HasSection, format!("sec:{}", doc.sections[p].heading), format!("sec:{sk}"), Value::Null),
            None => g.edge(EdgeKind::HasSection, &dk, format!("sec:{sk}"), Value::Null),
        }
        for &bi in &s.body {
            g.edge(EdgeKind::Contains, format!("sec:{sk}"), doc.blocks[bi].block.block_hk.as_str(), Value::Null);
        }
        let first_para = s.body.first().map(|&bi| doc.blocks[bi].block.text.as_str()).unwrap_or("");
        g.embed_targets.push(EmbedTarget { node_key: format!("sec:{sk}"), kind: NodeKind::Section, text: format!("{}\n{}", s.title, first_para) });
    }

    // Stitch links
    for l in &doc.links {
        let kind = match l.kind {
            LinkKind::ContinuesOn => EdgeKind::ContinuesOn,
            LinkKind::TableContinues => EdgeKind::TableContinues,
            LinkKind::CaptionOf => EdgeKind::CaptionOf,
            LinkKind::RefersTo => EdgeKind::RefersTo,
            LinkKind::Encloses => EdgeKind::EnclosesRegion,
            LinkKind::ValueOf => EdgeKind::ValueOf,
        };
        // `header_repeated` used to be dropped here, which lost the one fact a reader
        // merging two halves of a split table actually needs.
        let mut props = json!({ "confidence": l.confidence });
        if l.header_repeated {
            props["header_repeated"] = json!(true);
        }
        g.edge(kind, l.from.as_str(), l.to.as_str(), props);
    }

    // Entities (canonical key if resolved, else doc-local)
    let entity_key: HashMap<&str, String> = ext
        .entities
        .iter()
        .map(|e| (e.local_id.as_str(), e.entity_hk.as_ref().map(|h| h.to_string()).unwrap_or_else(|| format!("local:{dk}:{}", e.local_id))))
        .collect();
    let mention_entity: HashMap<&str, &str> =
        ext.entities.iter().flat_map(|e| e.mentions.iter().map(move |m| (m.as_str(), e.local_id.as_str()))).collect();
    for e in &ext.entities {
        let key = &entity_key[e.local_id.as_str()];
        g.node(NodeKind::Entity, key, json!({ "name": e.name, "type_id": e.type_id, "raw_type": e.raw_type, "aliases": e.aliases }));
        g.embed_targets.push(EmbedTarget { node_key: key.clone(), kind: NodeKind::Entity, text: format!("{} ({})", e.name, e.type_id.as_deref().unwrap_or(&e.raw_type)) });
    }
    for m in &ext.mentions {
        if let Some(local) = mention_entity.get(m.id.as_str()) {
            g.edge(EdgeKind::Mentions, m.block_hk.as_str(), &entity_key[local], json!({
                "char_start": m.char_start, "char_end": m.char_end, "surface": m.surface, "confidence": m.confidence,
            }));
        }
    }

    // Assertions
    for a in &ext.assertions {
        let entity_args: Vec<(&str, &String)> = a
            .args
            .iter()
            .filter_map(|arg| match &arg.value {
                ArgValue::Entity { local_id } => entity_key.get(local_id.as_str()).map(|k| (arg.role.as_str(), k)),
                ArgValue::Literal { .. } => None,
            })
            .collect();
        let literals: Vec<Value> = a
            .args
            .iter()
            .filter_map(|arg| match &arg.value {
                ArgValue::Literal { value, unit } => Some(json!({ "role": arg.role, "value": value, "unit": unit })),
                _ => None,
            })
            .collect();
        let ak = format!("asr:{dk}:{}", a.local_id);
        g.node(NodeKind::Assertion, &ak, json!({
            "predicate": a.predicate, "predicate_id": a.predicate_id, "kind": a.kind, "qualifiers": a.qualifiers,
            "confidence": a.confidence, "verified": a.verified, "literals": literals,
        }));
        for (role, ek) in &entity_args {
            g.edge(EdgeKind::Arg, &ak, ek.as_str(), json!({ "role": role }));
        }
        for ev in &a.evidence {
            match ev {
                Evidence::Span { block_hk, char_start, char_end } => {
                    g.edge(EdgeKind::Evidence, &ak, block_hk.as_str(), json!({ "char_start": char_start, "char_end": char_end }))
                }
                Evidence::Region { page_hk, bbox } => g.edge(EdgeKind::Evidence, &ak, page_hk.as_str(), json!({ "bbox": bbox })),
            }
        }
        if a.kind == AssertionKind::Binary {
            let subj = entity_args.iter().find(|(r, _)| *r == "subject");
            let obj = entity_args.iter().find(|(r, _)| *r == "object");
            if let (Some((_, s)), Some((_, o))) = (subj, obj) {
                g.edge(EdgeKind::Related, s.as_str(), o.as_str(), json!({ "predicate": a.predicate_id.as_ref().unwrap_or(&a.predicate), "assertion": &ak, "confidence": a.confidence }));
            }
        }

        // Citations get a direct block → entity edge as well as the assertion.
        //
        // `vs-extract`'s bibliography pass emits these as n-ary assertions with the
        // citing block as a *literal* `claim_block` and the reference as an entity, so
        // the reified form alone makes "what does this paper cite" a scan of every
        // assertion's arguments. The edge was declared and documented from the start and
        // never emitted; this is it.
        if a.predicate == "cites" {
            let claim_block = a.args.iter().find(|arg| arg.role == "claim_block").and_then(|arg| match &arg.value {
                ArgValue::Literal { value, .. } => Some(value.as_str()),
                ArgValue::Entity { .. } => None,
            });
            let reference = a.args.iter().find(|arg| arg.role == "reference").and_then(|arg| match &arg.value {
                ArgValue::Entity { local_id } => entity_key.get(local_id.as_str()).map(|k| k.as_str()),
                ArgValue::Literal { .. } => None,
            });
            if let (Some(from), Some(to)) = (claim_block, reference) {
                g.edge(EdgeKind::Cites, from, to, json!({ "assertion": &ak, "confidence": a.confidence }));
            }
        }
    }

    // In-image text last: matching needs every body block and entity already in hand.
    // The index folds and tokenises them once for the whole document.
    let entity_names: Vec<(String, String)> =
        ext.entities.iter().map(|e| (entity_key[e.local_id.as_str()].clone(), e.name.clone())).collect();
    let index = page::MatchIndex::build(body_text, entity_names);
    for (bk, content) in &image_blocks {
        page::add_image_text(&mut g, bk, content, &index);
    }
    g
}
