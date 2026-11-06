//! Document graph model and builder. See `SPEC.md`.

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
    SameAs,
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
}

impl DocGraph {
    fn node(&mut self, kind: NodeKind, key: impl Into<String>, props: Value) {
        self.nodes.push(GNode { kind, key: key.into(), props });
    }

    fn edge(&mut self, kind: EdgeKind, from: impl Into<String>, to: impl Into<String>, props: Value) {
        self.edges.push(GEdge { kind, from: from.into(), to: to.into(), props });
    }

    pub fn validate(&self) -> Result<(), GraphError> {
        let mut keys = HashSet::new();
        for n in &self.nodes {
            if !keys.insert((n.kind, n.key.as_str())) {
                return Err(GraphError::DuplicateNode(n.key.clone()));
            }
        }
        let all: HashSet<&str> = self.nodes.iter().map(|n| n.key.as_str()).collect();
        for e in &self.edges {
            if !all.contains(e.from.as_str()) || !all.contains(e.to.as_str()) {
                return Err(GraphError::DanglingEdge { kind: e.kind, from: e.from.clone(), to: e.to.clone() });
            }
        }
        for a in self.nodes.iter().filter(|n| n.kind == NodeKind::Assertion) {
            if !self.edges.iter().any(|e| e.kind == EdgeKind::Evidence && e.from == a.key) {
                return Err(GraphError::AssertionWithoutEvidence(a.key.clone()));
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
    for (i, b) in doc.blocks.iter().enumerate() {
        let bk = b.block.block_hk.as_str();
        g.node(NodeKind::Block, bk, json!({
            "kind": b.block.kind, "text": b.block.text, "bbox": b.block.bbox, "page_no": b.page_no,
            "global_order": b.global_order, "confidence": b.block.confidence, "content": b.block.content,
            "furniture": !doc.is_body(i),
        }));
        g.edge(EdgeKind::OnPage, bk, b.page_hk.as_str(), Value::Null);
        if doc.is_body(i) {
            if let Some(p) = prev_body {
                g.edge(EdgeKind::Next, p, bk, Value::Null);
            }
            prev_body = Some(bk);
            let embeddable = !b.block.text.trim().is_empty()
                && !matches!(b.block.kind, BlockKind::PageNumber | BlockKind::Header | BlockKind::Footer);
            if embeddable {
                g.embed_targets.push(EmbedTarget { node_key: bk.to_string(), kind: NodeKind::Block, text: b.block.text.clone() });
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
        };
        g.edge(kind, l.from.as_str(), l.to.as_str(), json!({ "confidence": l.confidence }));
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
                g.edge(EdgeKind::Related, s.as_str(), o.as_str(), json!({ "predicate": a.predicate_id.as_ref().unwrap_or(&a.predicate), "assertion": ak, "confidence": a.confidence }));
            }
        }
    }
    g
}
