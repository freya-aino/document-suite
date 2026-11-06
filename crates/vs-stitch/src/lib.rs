//! Stitching page results into one document. See `SPEC.md`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use vs_core::{Block, BlockKind, HashKey, PageResult, RunId};

pub mod xref;

pub use xref::{find_mentions, labels_for, link_cross_refs, Mention, RefLabel, RefTarget};

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
    /// `TableContinues` only: the continued table repeats the header row of the first
    /// (so readers merging the tables should drop it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub header_repeated: bool,
}

impl Link {
    pub fn new(kind: LinkKind, from: HashKey, to: HashKey, confidence: f32) -> Self {
        Link { kind, from, to, confidence, header_repeated: false }
    }
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

    /// Index into `blocks` of the block with this key.
    pub fn index_of(&self, key: &HashKey) -> Option<usize> {
        self.blocks.iter().position(|b| &b.block.block_hk == key)
    }

    /// Merged text of a chain of blocks (normally one from `continuation_chains`).
    /// Parts are joined with a space; a part ending in a hyphen is joined to a following
    /// part that starts with a lowercase letter without space or hyphen (de-hyphenation).
    /// Unknown keys are skipped.
    pub fn text_of(&self, chain: &[HashKey]) -> String {
        let mut out = String::new();
        for key in chain {
            let Some(i) = self.index_of(key) else { continue };
            let part = self.blocks[i].block.text.trim();
            if part.is_empty() {
                continue;
            }
            if out.is_empty() {
                out.push_str(part);
                continue;
            }
            let hyphenated = out.ends_with(['-', '\u{2010}', '\u{ad}']);
            let lower_start = part.chars().next().is_some_and(|c| c.is_lowercase());
            if hyphenated && lower_start {
                out.pop();
                out.push_str(part);
            } else {
                out.push(' ');
                out.push_str(part);
            }
        }
        out
    }

    /// Chains of blocks joined by `ContinuesOn` links, in reading order; each chain has at
    /// least two blocks. Blocks without continuation links are not listed.
    pub fn continuation_chains(&self) -> Vec<Vec<HashKey>> {
        let next: HashMap<&HashKey, &HashKey> = self.links.iter().filter(|l| l.kind == LinkKind::ContinuesOn).map(|l| (&l.from, &l.to)).collect();
        let has_prev: HashSet<&HashKey> = next.values().copied().collect();
        let mut chains = Vec::new();
        for b in &self.blocks {
            let head = &b.block.block_hk;
            if !next.contains_key(head) || has_prev.contains(head) {
                continue;
            }
            let mut chain = vec![head.clone()];
            let mut cur = head;
            let mut guard = HashSet::new();
            while let Some(&n) = next.get(cur) {
                if !guard.insert(n) {
                    break;
                }
                chain.push(n.clone());
                cur = n;
            }
            chains.push(chain);
        }
        chains
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

    let languages: HashMap<u32, String> = pages.iter().map(|p| (p.page_no, p.profile.language.primary())).collect();
    let mut doc = StitchedDocument { document_hk, run_id, blocks, furniture: HashSet::new(), sections: vec![], links: vec![], missing_pages };
    detect_furniture(&mut doc, pages.len());
    link_continuations(&mut doc);
    pair_captions(&mut doc);
    build_sections(&mut doc);
    xref::link_cross_refs(&mut doc, &languages);
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
            && counts.get(&norm(&b.block.text)).is_some_and(|s| s.len() * 2 >= n_pages);
        if by_kind || repeats {
            doc.furniture.insert(b.block.block_hk.clone());
        }
    }
}

fn ends_open(text: &str) -> bool {
    !text.trim_end().ends_with(['.', '!', '?', ':', ';', '。', '！', '？', '：', '；'])
}

fn starts_continuation(text: &str) -> bool {
    text.trim_start().chars().next().is_some_and(|c| c.is_lowercase() || matches!(c, ',' | ')' | '-'))
}

/// The first `<tr>…</tr>` of a table, lowercased.
fn first_row(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<tr")?;
    let end = lower[start..].find("</tr>").map_or(lower.len(), |e| start + e);
    Some(lower[start..end].to_string())
}

fn table_columns(html: &str) -> usize {
    first_row(html).map_or(0, |row| row.matches("<td").count() + row.matches("<th").count())
}

/// Cell texts of the first row, tags stripped and whitespace normalised.
fn header_cells(html: &str) -> Vec<String> {
    let Some(row) = first_row(html) else { return vec![] };
    let mut cells = Vec::new();
    let mut rest = row.as_str();
    while let Some(i) = rest.find("<t") {
        let after = &rest[i..];
        let is_cell = after.starts_with("<td") || after.starts_with("<th");
        let Some(close) = after.find('>') else { break };
        let after = &after[close + 1..];
        if !is_cell {
            rest = after;
            continue;
        }
        let end = after.find("</t").unwrap_or(after.len());
        cells.push(strip_tags(&after[..end]).split_whitespace().collect::<Vec<_>>().join(" "));
        rest = &after[end..];
    }
    cells
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// True when the continued table starts with the same header row as the first table.
fn repeats_header(first: &str, second: &str) -> bool {
    let (a, b) = (header_cells(first), header_cells(second));
    !a.is_empty() && a == b && a.iter().any(|c| !c.is_empty())
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
            links.push(Link::new(LinkKind::ContinuesOn, a.block.block_hk.clone(), b.block.block_hk.clone(), confidence));
        }
        if let (vs_core::BlockContent::Table { html: ha }, vs_core::BlockContent::Table { html: hb }) = (&a.block.content, &b.block.content) {
            let (ca, cb) = (table_columns(ha), table_columns(hb));
            if ca > 0 && ca == cb {
                let header_repeated = repeats_header(ha, hb);
                let confidence = if header_repeated { 0.9 } else { 0.85 };
                links.push(Link { kind: LinkKind::TableContinues, from: a.block.block_hk.clone(), to: b.block.block_hk.clone(), confidence, header_repeated });
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
            links.push(Link::new(LinkKind::CaptionOf, cap.block.block_hk.clone(), t.block.block_hk.clone(), 0.8));
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

    #[test]
    fn header_repeat_detection() {
        let a = "<table><tr><th>Name</th><th>Qty</th></tr><tr><td>x</td><td>1</td></tr></table>";
        let b = "<table><tr><th>Name</th><th> Qty </th></tr><tr><td>y</td><td>2</td></tr></table>";
        let c = "<table><tr><td>z</td><td>3</td></tr></table>";
        assert!(repeats_header(a, b));
        assert!(!repeats_header(a, c));
        assert_eq!(header_cells(a), vec!["name", "qty"]);
    }

    fn with_content(mut p: PageResult, i: usize, content: BlockContent) -> PageResult {
        p.blocks[i].content = content;
        p
    }

    #[test]
    fn table_continuation_marks_repeated_header() {
        let doc = HashKey::of(&["doc"]);
        let run = RunId::new();
        let html_a = "<table><tr><th>Name</th><th>Qty</th></tr><tr><td>x</td><td>1</td></tr></table>";
        let html_b = "<table><tr><th>Name</th><th>Qty</th></tr><tr><td>y</td><td>2</td></tr></table>";
        let p1 = with_content(page(&doc, run, 1, vec![(BlockKind::Table, "", 0.5)]), 0, BlockContent::Table { html: html_a.into() });
        let p2 = with_content(page(&doc, run, 2, vec![(BlockKind::Table, "", 0.1)]), 0, BlockContent::Table { html: html_b.into() });
        let d = stitch(vec![p1, p2], 2).unwrap();
        let l = d.links.iter().find(|l| l.kind == LinkKind::TableContinues).expect("table link");
        assert!(l.header_repeated);
        // serde: field is optional on read, omitted when false
        let json = serde_json::to_string(&Link::new(LinkKind::CaptionOf, l.from.clone(), l.to.clone(), 0.5)).unwrap();
        assert!(!json.contains("header_repeated"));
        let back: Link = serde_json::from_str(&json).unwrap();
        assert!(!back.header_repeated);
    }

    #[test]
    fn text_of_and_continuation_chains() {
        let doc = HashKey::of(&["doc"]);
        let run = RunId::new();
        let p1 = page(&doc, run, 1, vec![(BlockKind::Paragraph, "Models that parse docu-", 0.3)]);
        let p2 = page(&doc, run, 2, vec![(BlockKind::Paragraph, "ments need stitching", 0.1)]);
        let p3 = page(&doc, run, 3, vec![(BlockKind::Paragraph, "across pages.", 0.1), (BlockKind::Paragraph, "Separate.", 0.5)]);
        let d = stitch(vec![p1, p2, p3], 3).unwrap();
        let chains = d.continuation_chains();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 3);
        assert_eq!(d.text_of(&chains[0]), "Models that parse documents need stitching across pages.");
        assert_eq!(d.text_of(&[d.blocks[3].block.block_hk.clone()]), "Separate.");
        assert_eq!(d.text_of(&[HashKey::of(&["nope"])]), "");
    }

    #[test]
    fn cross_references_resolve_to_captioned_blocks_formulas_and_headings() {
        let doc = HashKey::of(&["doc"]);
        let run = RunId::new();
        let p1 = page(&doc, run, 1, vec![
            (BlockKind::Heading, "1 Introduction", 0.05),
            (BlockKind::Paragraph, "Results are in Table 1 and Fig. 2a; see Eq. (3) and Section 2.1. Table 9 does not exist.", 0.15),
            (BlockKind::Table, "", 0.3),
            (BlockKind::Caption, "Table 1: Results", 0.4),
            (BlockKind::Formula, "", 0.5),
            (BlockKind::Formula, "", 0.6),
        ]);
        let p1 = with_content(p1, 2, BlockContent::Table { html: "<table><tr><td>a</td></tr></table>".into() });
        let p1 = with_content(p1, 4, BlockContent::Formula { latex: "E = mc^2 \\tag{3}".into() });
        let p1 = with_content(p1, 5, BlockContent::Formula { latex: "a^2 + b^2 = c^2 \\tag{4}".into() });
        let p2 = page(&doc, run, 2, vec![
            (BlockKind::Heading, "2 Method", 0.05),
            (BlockKind::Heading, "2.1 Setup", 0.1),
            (BlockKind::Figure, "", 0.3),
            (BlockKind::Caption, "Figure 2. Two panels", 0.5),
            (BlockKind::Paragraph, "Compare Eq. 7 with the rest.", 0.6),
        ]);
        let d = stitch(vec![p1, p2], 2).unwrap();
        let key = |page: u32, i: u32| HashKey::block(&HashKey::page(&doc, page), &run, i);
        let refs: Vec<&Link> = d.links.iter().filter(|l| l.kind == LinkKind::RefersTo).collect();
        let from_para = |l: &&&Link| l.from == key(1, 1);
        let targets: HashSet<HashKey> = refs.iter().filter(from_para).map(|l| l.to.clone()).collect();
        // Table 1 → the table block (via CaptionOf), Fig. 2a → the figure block, Eq. (3) → formula, Section 2.1 → heading
        assert_eq!(targets, HashSet::from([key(1, 2), key(2, 2), key(1, 4), key(2, 1)]));
        assert!(refs.iter().filter(from_para).all(|l| l.confidence == 0.8));
        // Eq. 7 has no numbered formula → nearest formula with fallback confidence
        let eq7 = refs.iter().find(|l| l.from == key(2, 4)).expect("fallback link");
        assert_eq!(eq7.to, key(1, 5));
        assert_eq!(eq7.confidence, xref::CONFIDENCE_FALLBACK);
        // no self links, and the unresolvable "Table 9" produced nothing extra
        assert!(d.links.iter().all(|l| l.from != l.to));
        assert_eq!(refs.len(), 5);
    }

    #[test]
    fn ambiguous_cross_reference_picks_nearest() {
        let doc = HashKey::of(&["doc"]);
        let run = RunId::new();
        let p1 = page(&doc, run, 1, vec![(BlockKind::Caption, "Table 1: first", 0.2), (BlockKind::Paragraph, "x", 0.5)]);
        let p2 = page(&doc, run, 2, vec![(BlockKind::Paragraph, "See Table 1.", 0.2), (BlockKind::Caption, "Table 1: again", 0.3)]);
        let d = stitch(vec![p1, p2], 2).unwrap();
        let refs: Vec<&Link> = d.links.iter().filter(|l| l.kind == LinkKind::RefersTo).collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to, HashKey::block(&HashKey::page(&doc, 2), &run, 1));
        assert_eq!(refs[0].confidence, 0.5);
    }

    #[test]
    fn cross_references_use_page_language() {
        let doc = HashKey::of(&["doc"]);
        let run = RunId::new();
        let mut p1 = page(&doc, run, 1, vec![(BlockKind::Paragraph, "Siehe Abb. 1.", 0.2), (BlockKind::Caption, "Abbildung 1: Aufbau", 0.5)]);
        p1.profile.language = LanguageTag::parse("de-CH").unwrap();
        let d = stitch(vec![p1], 1).unwrap();
        let refs: Vec<&Link> = d.links.iter().filter(|l| l.kind == LinkKind::RefersTo).collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].to, HashKey::block(&HashKey::page(&doc, 1), &run, 1));
    }
}
