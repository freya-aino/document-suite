//! Test fixtures: hand-built `StitchedDocument`s without going through vs-stitch.

use std::collections::HashSet;

use vs_core::{BBox, Block, BlockContent, BlockKind, HashKey, Provenance, RunId};
use vs_stitch::{DocBlock, Link, LinkKind, Section, StitchedDocument};

pub fn prov(run: RunId) -> Provenance {
    Provenance {
        run_id: run,
        dag_ref: "t@1".into(),
        node_id: "n".into(),
        step_kind: "s".into(),
        model_ref: None,
        prompt_ref: None,
        params_hash: "0".into(),
        started_at: chrono::DateTime::parse_from_rfc3339("2026-11-06T00:00:00Z").unwrap().with_timezone(&chrono::Utc),
        duration_ms: 0,
    }
}

/// Builder for a one-page document: `(kind, text)` per block, headings open sections.
pub struct DocBuilder {
    pub doc: StitchedDocument,
    page_hk: HashKey,
}

impl DocBuilder {
    pub fn new() -> Self {
        let document_hk = HashKey::of(&["doc"]);
        let run_id = RunId::new();
        let page_hk = HashKey::page(&document_hk, 1);
        Self {
            doc: StitchedDocument {
                document_hk,
                run_id,
                blocks: vec![],
                furniture: HashSet::new(),
                sections: vec![],
                links: vec![],
                missing_pages: vec![],
            },
            page_hk,
        }
    }

    pub fn block(&mut self, kind: BlockKind, text: &str) -> usize {
        self.block_with(kind, text, BlockContent::Text { text: text.into() })
    }

    pub fn block_with(&mut self, kind: BlockKind, text: &str, content: BlockContent) -> usize {
        let i = self.doc.blocks.len();
        let y = (i as f32 * 0.05).min(0.9);
        let block = Block {
            block_hk: HashKey::block(&self.page_hk, &self.doc.run_id, i as u32),
            index: i as u32,
            kind,
            bbox: BBox::new(0.1, y, 0.9, y + 0.05).unwrap(),
            reading_order: i as u32,
            content,
            text: text.into(),
            level: None,
            confidence: 0.9,
            provenance: prov(self.doc.run_id),
        };
        self.doc.blocks.push(DocBlock { block, page_no: 1, page_hk: self.page_hk.clone(), global_order: i as u32 });
        if kind == BlockKind::Heading || kind == BlockKind::Title {
            let level = if kind == BlockKind::Title { 0 } else { 1 };
            let parent = self.doc.sections.iter().rposition(|s| s.level < level);
            self.doc.sections.push(Section { heading: self.hk(i), level, title: text.into(), body: vec![], parent });
        } else if let Some(s) = self.doc.sections.last_mut() {
            s.body.push(i);
        }
        i
    }

    pub fn link(&mut self, kind: LinkKind, from: usize, to: usize) {
        // Built via serde so the fixture does not depend on `Link`'s exact field set.
        let link: Link = serde_json::from_value(serde_json::json!({ "kind": kind, "from": self.hk(from), "to": self.hk(to), "confidence": 0.9 })).unwrap();
        self.doc.links.push(link);
    }

    pub fn hk(&self, i: usize) -> HashKey {
        self.doc.blocks[i].block.block_hk.clone()
    }
}
