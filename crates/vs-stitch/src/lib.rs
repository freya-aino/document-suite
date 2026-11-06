//! Stitching page results into one document. See `SPEC.md`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use vs_core::{Block, BlockKind, HashKey, PageResult, RunId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocBlock {
    pub block: Block,
    pub page_no: u32,
    pub page_hk: HashKey,
    pub global_order: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    ContinuesOn,
    TableContinues,
    CaptionOf,
    RefersTo,
}

/// A stitching decision between two blocks (`from` → `to`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub kind: LinkKind,
    pub from: HashKey,
    pub to: HashKey,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub heading: HashKey,
    pub level: u8,
    pub title: String,
    /// Indices into `StitchedDocument::blocks` belonging to this section (excluding sub-sections' headings).
    pub body: Vec<usize>,
    pub parent: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StitchedDocument {
    pub document_hk: HashKey,
    pub run_id: RunId,
    pub blocks: Vec<DocBlock>,
    pub furniture: HashSet<HashKey>,
    pub sections: Vec<Section>,
    pub links: Vec<Link>,
    pub missing_pages: Vec<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum StitchError {
    #[error("no pages to stitch")]
    Empty,
    #[error("page results belong to different documents or runs")]
    Mixed,
}

impl StitchedDocument {
    pub fn is_body(&self, i: usize) -> bool {
        !self.furniture.contains(&self.blocks[i].block.block_hk)
    }
}

/// Stitches pages. `pages_total` is used to detect missing pages.
pub fn stitch(mut pages: Vec<PageResult>, pages_total: u32) -> Result<StitchedDocument, StitchError> {
    let first = pages.first().ok_or(StitchError::Empty)?;
    let (document_hk, run_id) = (first.document_hk.clone(), first.run_id);
    if pages.iter().any(|p| p.document_hk != document_hk || p.run_id != run_id) {
        return Err(StitchError::Mixed);
    }
    pages.sort_by_key(|p| p.page_no);
    let present: HashSet<u32> = pages.iter().map(|p| p.page_no).collect();
    let missing_pages = (1..=pages_total).filter(|n| !present.contains(n)).collect();

    // 1. global order
    let mut blocks = Vec::new();
    for p in &pages {
        let mut bs = p.blocks.clone();
        bs.sort_by_key(|b| b.reading_order);
        for b in bs {
            let global_order = blocks.len() as u32;
            blocks.push(DocBlock { block: b, page_no: p.page_no, page_hk: p.page_hk.clone(), global_order });
        }
    }

    let mut doc = StitchedDocument { document_hk, run_id, blocks, furniture: HashSet::new(), sections: vec![], links: vec![], missing_pages };
    detect_furniture(&mut doc, pages.len());
    link_continuations(&mut doc);
    pair_captions(&mut doc);
    build_sections(&mut doc);
    // Cross references: see SPEC.md §7 (todo).
    Ok(doc)
}

fn detect_furniture(doc: &mut StitchedDocument, n_pages: usize) {
    let norm = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>().trim().to_lowercase();
    let mut counts: HashMap<String, HashSet<u32>> = HashMap::new();
    for b in &doc.blocks {
        if !b.block.text.is_empty() {
            counts.entry(norm(&b.block.text)).or_default().insert(b.page_no);
        }
    }
    for b in &doc.blocks {
        let by_kind = matches!(b.block.kind, BlockKind::Header | BlockKind::Footer | BlockKind::PageNumber);
        let near_edge = b.block.bbox.y1 < 0.1 || b.block.bbox.y0 > 0.9;
        let repeats = n_pages >= 3
            && near_edge
            && counts.get(&norm(&b.block.text)).map_or(false, |s| s.len() * 2 >= n_pages);
        if by_kind || repeats {
            doc.furniture.insert(b.block.block_hk.clone());
        }
    }
}

fn ends_open(text: &str) -> bool {
    !text.trim_end().ends_with(['.', '!', '?', ':', ';', '。', '！', '？', '：', '；'])
}

fn starts_continuation(text: &str) -> bool {
    text.trim_start().chars().next().map_or(false, |c| c.is_lowercase() || matches!(c, ',' | ')' | '-'))
}

fn table_columns(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<tr") else { return 0 };
    let end = lower[start..].find("</tr>").map_or(lower.len(), |e| start + e);
    let row = &lower[start..end];
    row.matches("<td").count() + row.matches("<th").count()
}

fn link_continuations(doc: &mut StitchedDocument) {
    let body: Vec<usize> = (0..doc.blocks.len()).filter(|&i| doc.is_body(i)).collect();
    let mut links = vec![];
    for w in body.windows(2) {
        let (a, b) = (&doc.blocks[w[0]], &doc.blocks[w[1]]);
        if b.page_no != a.page_no + 1 {
            continue; // same page or a gap
        }
        let text_kind = |k: BlockKind| matches!(k, BlockKind::Paragraph | BlockKind::List);
        if text_kind(a.block.kind) && text_kind(b.block.kind) && ends_open(&a.block.text) {
            let confidence = if starts_continuation(&b.block.text) { 0.9 } else { 0.6 };
            links.push(Link { kind: LinkKind::ContinuesOn, from: a.block.block_hk.clone(), to: b.block.block_hk.clone(), confidence });
        }
        if let (vs_core::BlockContent::Table { html: ha }, vs_core::BlockContent::Table { html: hb }) = (&a.block.content, &b.block.content) {
            let (ca, cb) = (table_columns(ha), table_columns(hb));
            if ca > 0 && ca == cb {
                links.push(Link { kind: LinkKind::TableContinues, from: a.block.block_hk.clone(), to: b.block.block_hk.clone(), confidence: 0.85 });
            }
        }
    }
    doc.links.extend(links);
}

fn pair_captions(doc: &mut StitchedDocument) {
    let mut links = vec![];
    for cap in doc.blocks.iter().filter(|b| b.block.kind == BlockKind::Caption) {
        let best = doc
            .blocks
            .iter()
            .filter(|t| t.page_no == cap.page_no && matches!(t.block.kind, BlockKind::Figure | BlockKind::Table | BlockKind::Chart))
            .min_by(|x, y| {
                let d = |t: &DocBlock| {
                    let tc = (t.block.bbox.y0 + t.block.bbox.y1) / 2.0;
                    let cc = (cap.block.bbox.y0 + cap.block.bbox.y1) / 2.0;
                    (tc - cc).abs()
                };
                d(x).partial_cmp(&d(y)).unwrap_or(std::cmp::Ordering::Equal)
            });
        if let Some(t) = best {
            links.push(Link { kind: LinkKind::CaptionOf, from: cap.block.block_hk.clone(), to: t.block.block_hk.clone(), confidence: 0.8 });
        }
    }
    doc.links.extend(links);
}

/// Heading level from numbering like "2.3.1 Results" when the model gave none.
fn numbering_level(title: &str) -> Option<u8> {
    let token = title.split_whitespace().next()?;
    let token = token.trim_end_matches('.');
    let parts: Vec<&str> = token.split('.').collect();
    if !parts.is_empty() && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        Some(parts.len().min(6) as u8)
    } else {
        None
    }
}

fn build_sections(doc: &mut StitchedDocument) {
    let mut sections: Vec<Section> = vec![];
    let mut stack: Vec<usize> = vec![]; // indices into sections
    for i in 0..doc.blocks.len() {
        if !doc.is_body(i) {
            continue;
        }
        let b = &doc.blocks[i].block;
        if matches!(b.kind, BlockKind::Heading | BlockKind::Title) {
            let level = if b.kind == BlockKind::Title { 0 } else { b.level.or_else(|| numbering_level(&b.text)).unwrap_or(1) };
            while let Some(&top) = stack.last() {
                if sections[top].level >= level {
                    stack.pop();
                } else {
                    break;
                }
            }
            sections.push(Section { heading: b.block_hk.clone(), level, title: b.text.clone(), body: vec![], parent: stack.last().copied() });
            stack.push(sections.len() - 1);
        } else if let Some(&top) = stack.last() {
            sections[top].body.push(i);
        }
    }
    doc.sections = sections;
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::{BBox, BlockContent, ClassifierTier, DocType, LanguageTag, PageProfile, Provenance};

    fn prov(run: RunId) -> Provenance {
        serde_json::from_value(serde_json::json!({
            "run_id": run, "dag_ref": "t@1", "node_id": "n", "step_kind": "s", "model_ref": null,
            "prompt_ref": null, "params_hash": "0", "started_at": "2026-11-06T00:00:00Z", "duration_ms": 0
        }))
        .unwrap()
    }

    fn block(page_hk: &HashKey, run: RunId, i: u32, kind: BlockKind, text: &str, y: f32) -> Block {
        Block {
            block_hk: HashKey::block(page_hk, &run, i),
            index: i,
            kind,
            bbox: BBox::new(0.1, y, 0.9, y + 0.05).unwrap(),
            reading_order: i,
            content: BlockContent::Text { text: text.into() },
            text: text.into(),
            level: None,
            confidence: 0.9,
            provenance: prov(run),
        }
    }

    fn page(doc: &HashKey, run: RunId, n: u32, blocks: Vec<(BlockKind, &str, f32)>) -> PageResult {
        let page_hk = HashKey::page(doc, n);
        PageResult {
            schema_version: 1,
            document_hk: doc.clone(),
            page_hk: page_hk.clone(),
            page_no: n,
            run_id: run,
            render_width_px: 1000,
            render_height_px: 1400,
            profile: PageProfile {
                language: LanguageTag::parse("en").unwrap(),
                doc_type: DocType::ScientificPaper,
                born_digital: true,
                text_layer_coverage: 1.0,
                has_handwriting: false,
                has_images: false,
                has_tables: false,
                has_formulas: false,
                has_charts: false,
                rotation_deg: 0,
                confidence: 1.0,
                tier: ClassifierTier::Rules,
            },
            dag_ref: "t@1".into(),
            blocks: blocks.into_iter().enumerate().map(|(i, (k, t, y))| block(&page_hk, run, i as u32, k, t, y)).collect(),
        }
    }

    #[test]
    fn stitches_continuation_sections_and_missing_pages() {
        let doc = HashKey::of(&["doc"]);
        let run = RunId::new();
        let p1 = page(&doc, run, 1, vec![
            (BlockKind::Heading, "1 Introduction", 0.1),
            (BlockKind::Paragraph, "We study how models", 0.3),
        ]);
        let p2 = page(&doc, run, 2, vec![
            (BlockKind::Paragraph, "parse documents.", 0.1),
            (BlockKind::Heading, "1.1 Scope", 0.3),
            (BlockKind::Paragraph, "Scope text.", 0.4),
        ]);
        let d = stitch(vec![p2, p1], 3).unwrap();
        assert_eq!(d.missing_pages, vec![3]);
        assert_eq!(d.blocks[0].block.text, "1 Introduction");
        let cont: Vec<_> = d.links.iter().filter(|l| l.kind == LinkKind::ContinuesOn).collect();
        assert_eq!(cont.len(), 1);
        assert_eq!(cont[0].confidence, 0.9);
        assert_eq!(d.sections.len(), 2);
        assert_eq!(d.sections[1].level, 2);
        assert_eq!(d.sections[1].parent, Some(0));
    }

    #[test]
    fn table_columns_counts_first_row() {
        assert_eq!(table_columns("<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>"), 2);
    }
}
