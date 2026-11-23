//! Stitching page results into one document. See `SPEC.md`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use vs_core::{Block, BlockKind, HashKey, PageResult, RunId};

pub mod page_layout;
pub mod xref;

pub use page_layout::{column_order, columns_of, enclosures, key_values, repair_reading_order};
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
    /// `from` geometrically encloses `to`, on the same page.
    Encloses,
    /// `from` is the value of the label `to`, on a form-like page.
    ValueOf,
    /// `from` is a footnote and `to` is the body block whose text carries its marker.
    FootnoteOf,
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
    // Reading order first: sections, continuations and cross-references all read
    // `global_order`, so repairing it afterwards would leave them describing an order
    // that no longer exists.
    let repaired = page_layout::repair_reading_order(&mut doc);
    if !repaired.is_empty() {
        tracing::debug!(pages = ?repaired, "multi-column reading order repaired");
    }
    link_continuations(&mut doc);
    pair_captions(&mut doc);
    anchor_footnotes(&mut doc);
    let page_types: HashMap<u32, vs_core::DocType> = pages.iter().map(|p| (p.page_no, p.profile.doc_type)).collect();
    doc.links.extend(page_layout::enclosures(&doc));
    doc.links.extend(page_layout::key_values(&doc, &page_types));
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

/// A caption and its target must share this much of the narrower one's width to be read
/// as belonging together. A caption sits under (or over) the thing it describes, so it is
/// aligned with it; two items side by side are not a pair however close their centres are.
const CAPTION_OVERLAP: f32 = 0.3;

/// A caption further than this from its target, as a fraction of the page height, is not
/// its caption. Without a bound, a page with one figure at the top and one stray caption
/// at the bottom pairs them for lack of anything better.
const MAX_CAPTION_GAP: f32 = 0.25;

/// `CaptionOf` links: each caption to the figure, chart or table it describes.
///
/// Three things decide it, in the order they matter:
///
/// 1. **Alignment.** A caption is printed across the width of its target, so a candidate
///    that does not overlap it horizontally is not a candidate at all. This is what the
///    earlier version was missing — it minimised the distance between vertical *centres*
///    with no horizontal test, so on a page with two figures side by side a caption under
///    the left one could be paired with the right one.
/// 2. **The gap between the boxes**, not between their centres. A tall figure's centre is
///    far from its own caption, and a short one next to it is nearer — by centre distance
///    the caption goes to the wrong figure, by edge gap it does not.
/// 3. **Which side it is on.** A figure's caption goes below it and a table's above it,
///    almost everywhere, and the convention is strong enough to break a tie and to be
///    worth saying in the confidence when it is violated.
fn pair_captions(doc: &mut StitchedDocument) {
    let mut links = vec![];
    for cap in doc.blocks.iter().filter(|b| b.block.kind == BlockKind::Caption) {
        let cb = &cap.block.bbox;
        let mut best: Option<(&DocBlock, f32, bool)> = None;
        for t in doc
            .blocks
            .iter()
            .filter(|t| t.page_no == cap.page_no && matches!(t.block.kind, BlockKind::Figure | BlockKind::Table | BlockKind::Chart))
        {
            let tb = &t.block.bbox;
            if page_layout::horizontal_overlap(cb, tb) < CAPTION_OVERLAP {
                continue;
            }
            // Vertical gap between the two boxes; zero when they touch or overlap.
            let gap = (cb.y0 - tb.y1).max(tb.y0 - cb.y1).max(0.0);
            if gap > MAX_CAPTION_GAP {
                continue;
            }
            // Below a figure or a chart, above a table.
            let caption_below = cb.y0 >= (tb.y0 + tb.y1) / 2.0;
            let conventional = match t.block.kind {
                BlockKind::Table => !caption_below,
                _ => caption_below,
            };
            // The convention breaks ties rather than overriding geometry: a small penalty,
            // a tenth of the allowed gap, so a clearly nearer target on the unusual side
            // still wins.
            let score = gap + if conventional { 0.0 } else { MAX_CAPTION_GAP / 10.0 };
            if best.is_none_or(|(_, s, _)| score < s) {
                best = Some((t, score, conventional));
            }
        }
        if let Some((t, _, conventional)) = best {
            // Said in the confidence rather than hidden: a caption on the unusual side of
            // its target is still the best reading available, and a reader thresholding
            // on this can tell the two cases apart.
            let confidence = if conventional { 0.85 } else { 0.6 };
            links.push(Link::new(LinkKind::CaptionOf, cap.block.block_hk.clone(), t.block.block_hk.clone(), confidence));
        }
    }
    doc.links.extend(links);
}

/// Markers a footnote can be numbered with, beyond plain digits.
///
/// The superscript digits are here because a text layer often carries them literally,
/// and a model transcribing a page may emit either form — so both the footnote's own
/// marker and the one in the body text are normalised through [`marker_of`].
const FOOTNOTE_SYMBOLS: [char; 6] = ['*', '†', '‡', '§', '¶', '#'];

/// The marker a footnote begins with: `12` from "12. Jones (2024)", `†` from "† ibid".
///
/// `None` when the footnote starts with no marker at all, which is common enough —
/// a model that drops the superscript gives the text alone — and is why anchoring is
/// best-effort rather than an invariant.
fn marker_of(text: &str) -> Option<String> {
    let t = text.trim_start();
    let first = t.chars().next()?;
    if FOOTNOTE_SYMBOLS.contains(&first) {
        // A run of the same symbol: `*`, `**`, `***` number footnotes on many pages.
        let run: String = t.chars().take_while(|c| *c == first).collect();
        return Some(run);
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit() || superscript_digit(*c).is_some()).collect();
    if digits.is_empty() {
        return None;
    }
    let normalised: String = digits.chars().map(|c| superscript_digit(c).unwrap_or(c)).collect();
    // A footnote numbered past three digits is almost certainly a year or an amount that
    // happens to lead the line.
    (normalised.len() <= 3).then_some(normalised)
}

/// `²` → `2`.
fn superscript_digit(c: char) -> Option<char> {
    const SUPERSCRIPTS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    SUPERSCRIPTS.iter().position(|s| *s == c).map(|i| (b'0' + i as u8) as char)
}

/// Whether `text` carries `marker` as a reference marker rather than as part of a word or
/// a number.
///
/// The hard case is a digit: "3" appears in "Table 3", in "1,300" and in a hundred other
/// places on a page, so a bare search would anchor every footnote to the first paragraph
/// containing its number. Two forms are accepted, and nothing else: the superscript
/// (`text³`), which is unambiguous, and a digit in brackets (`[3]`, `(3)`). A symbol
/// marker needs no such care — `†` is a footnote marker wherever it appears.
fn carries_marker(text: &str, marker: &str) -> bool {
    if marker.starts_with(|c: char| FOOTNOTE_SYMBOLS.contains(&c)) {
        return text.contains(marker);
    }
    let superscript: String = marker
        .chars()
        .filter_map(|c| c.to_digit(10).map(|d| ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'][d as usize]))
        .collect();
    if !superscript.is_empty() && text.contains(&superscript) {
        return true;
    }
    text.contains(&format!("[{marker}]")) || text.contains(&format!("({marker})"))
}

/// `FootnoteOf` links: each footnote to the body block whose text carries its marker.
///
/// Footnotes were ordered with everything else and linked to nothing, so the sentence a
/// footnote belongs to was recoverable only by a reader doing this themselves. Anchoring
/// is per page, because a footnote is printed on the page of its reference, and prefers
/// the **last** body block before the footnote that carries the marker: a marker repeated
/// down a page refers forward to the nearest footnote below it.
fn anchor_footnotes(doc: &mut StitchedDocument) {
    let mut links = vec![];
    for (fi, fnote) in doc.blocks.iter().enumerate().filter(|(_, b)| b.block.kind == BlockKind::Footnote) {
        let Some(marker) = marker_of(&fnote.block.text) else { continue };
        let anchor = doc
            .blocks
            .iter()
            .enumerate()
            .filter(|(i, b)| {
                *i != fi
                    && b.page_no == fnote.page_no
                    && b.block.kind != BlockKind::Footnote
                    && doc.is_body(*i)
                    && matches!(b.block.kind, BlockKind::Paragraph | BlockKind::List | BlockKind::Heading | BlockKind::Table | BlockKind::Caption)
            })
            .filter(|(_, b)| carries_marker(&b.block.text, &marker))
            .max_by_key(|(_, b)| b.global_order);
        if let Some((_, b)) = anchor {
            links.push(Link::new(LinkKind::FootnoteOf, fnote.block.block_hk.clone(), b.block.block_hk.clone(), 0.8));
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
    /// A block at an arbitrary box, for the geometric tests.
    fn placed(page_hk: &HashKey, run: RunId, i: u32, kind: BlockKind, text: &str, bbox: BBox) -> Block {
        Block { bbox, ..block(page_hk, run, i, kind, text, 0.0) }
    }

    fn doc_of(blocks: Vec<Block>, run: RunId, doc: &HashKey) -> StitchedDocument {
        let page_hk = HashKey::page(doc, 1);
        let blocks = blocks
            .into_iter()
            .enumerate()
            .map(|(i, block)| DocBlock { block, page_no: 1, page_hk: page_hk.clone(), global_order: i as u32 })
            .collect();
        StitchedDocument {
            document_hk: doc.clone(),
            run_id: run,
            blocks,
            furniture: HashSet::new(),
            sections: vec![],
            links: vec![],
            missing_pages: vec![],
        }
    }

    fn bb(x0: f32, y0: f32, x1: f32, y1: f32) -> BBox {
        BBox::new(x0, y0, x1, y1).expect("bbox")
    }

    /// Two figures side by side, one caption under the left one. By vertical-centre
    /// distance alone — which is all this used to measure — the caption can land on the
    /// right figure; alignment is what decides it.
    #[test]
    fn a_caption_goes_to_the_figure_it_is_aligned_with() {
        let (run, doc) = (RunId::new(), HashKey::of(&["d"]));
        let page_hk = HashKey::page(&doc, 1);
        let blocks = vec![
            // Left figure: tall, so its centre is far from its own caption.
            placed(&page_hk, run, 0, BlockKind::Figure, "", bb(0.05, 0.10, 0.45, 0.60)),
            // Right figure: short, and its centre is nearer the caption below the left one.
            placed(&page_hk, run, 1, BlockKind::Figure, "", bb(0.55, 0.55, 0.95, 0.60)),
            placed(&page_hk, run, 2, BlockKind::Caption, "Figure 1: the left one", bb(0.05, 0.62, 0.45, 0.66)),
        ];
        let mut d = doc_of(blocks, run, &doc);
        pair_captions(&mut d);
        let link = d.links.iter().find(|l| l.kind == LinkKind::CaptionOf).expect("a caption link");
        assert_eq!(link.to, HashKey::block(&page_hk, &run, 0), "the caption was paired with the figure it is not under");
        assert!(link.confidence > 0.8, "a caption below a figure is the conventional side");
    }

    #[test]
    fn a_table_caption_above_its_table_is_conventional_and_one_below_is_doubted() {
        let (run, doc) = (RunId::new(), HashKey::of(&["d"]));
        let page_hk = HashKey::page(&doc, 1);
        let above = vec![
            placed(&page_hk, run, 0, BlockKind::Caption, "Table 1: results", bb(0.1, 0.10, 0.9, 0.14)),
            placed(&page_hk, run, 1, BlockKind::Table, "", bb(0.1, 0.16, 0.9, 0.40)),
        ];
        let mut d = doc_of(above, run, &doc);
        pair_captions(&mut d);
        assert!(d.links[0].confidence > 0.8, "a table's caption belongs above it");

        let below = vec![
            placed(&page_hk, run, 0, BlockKind::Table, "", bb(0.1, 0.16, 0.9, 0.40)),
            placed(&page_hk, run, 1, BlockKind::Caption, "Table 1: results", bb(0.1, 0.42, 0.9, 0.46)),
        ];
        let mut d = doc_of(below, run, &doc);
        pair_captions(&mut d);
        // Still the right answer — it is the only table on the page — but said with less
        // confidence, which is the difference a reader can act on.
        assert_eq!(d.links.len(), 1);
        assert!(d.links[0].confidence < 0.8, "an unconventional side is not stated as confidently");
    }

    #[test]
    fn a_stray_caption_is_paired_with_nothing() {
        let (run, doc) = (RunId::new(), HashKey::of(&["d"]));
        let page_hk = HashKey::page(&doc, 1);
        let blocks = vec![
            placed(&page_hk, run, 0, BlockKind::Figure, "", bb(0.05, 0.05, 0.45, 0.15)),
            // Half a page away, and not aligned with it either.
            placed(&page_hk, run, 1, BlockKind::Caption, "Figure 7: from another page", bb(0.55, 0.90, 0.95, 0.95)),
        ];
        let mut d = doc_of(blocks, run, &doc);
        pair_captions(&mut d);
        assert!(d.links.is_empty(), "a caption was paired with a figure it cannot belong to");
    }

    #[test]
    fn a_footnote_is_anchored_to_the_text_carrying_its_marker() {
        let (run, doc) = (RunId::new(), HashKey::of(&["d"]));
        let page_hk = HashKey::page(&doc, 1);
        let blocks = vec![
            placed(&page_hk, run, 0, BlockKind::Paragraph, "The rate was 3 per cent in 1970.", bb(0.1, 0.10, 0.9, 0.20)),
            placed(&page_hk, run, 1, BlockKind::Paragraph, "The second estimate² is the one used here.", bb(0.1, 0.25, 0.9, 0.35)),
            placed(&page_hk, run, 2, BlockKind::Footnote, "2. Jones (2024), p. 31.", bb(0.1, 0.90, 0.9, 0.94)),
        ];
        let mut d = doc_of(blocks, run, &doc);
        anchor_footnotes(&mut d);
        let link = d.links.iter().find(|l| l.kind == LinkKind::FootnoteOf).expect("a footnote link");
        assert_eq!(link.from, HashKey::block(&page_hk, &run, 2));
        // Not the first paragraph, which contains a bare "3" and a year: a digit on a page
        // is not a marker unless it is marked as one.
        assert_eq!(link.to, HashKey::block(&page_hk, &run, 1), "anchored to a paragraph that merely contains the digit");
    }

    #[test]
    fn footnote_markers_are_read_in_the_forms_a_page_uses() {
        assert_eq!(marker_of("12. Jones"), Some("12".into()));
        assert_eq!(marker_of("² ibid"), Some("2".into()));
        assert_eq!(marker_of("† the same"), Some("†".into()));
        assert_eq!(marker_of("** and again"), Some("**".into()));
        assert_eq!(marker_of("See also Jones"), None, "no marker at all is a footnote we cannot anchor");
        assert_eq!(marker_of("1970 was different"), None, "a year is not a marker");

        assert!(carries_marker("as shown³ here", "3"), "a superscript is unambiguous");
        assert!(carries_marker("as shown [3] here", "3"));
        assert!(carries_marker("as shown (3) here", "3"));
        assert!(!carries_marker("Table 3 shows", "3"), "a bare digit must not anchor a footnote");
        assert!(!carries_marker("1,300 units", "3"));
        assert!(carries_marker("the usual caveat† applies", "†"));
    }

    #[test]
    fn a_symbol_footnote_without_a_marker_in_the_text_anchors_to_nothing() {
        let (run, doc) = (RunId::new(), HashKey::of(&["d"]));
        let page_hk = HashKey::page(&doc, 1);
        let blocks = vec![
            placed(&page_hk, run, 0, BlockKind::Paragraph, "A paragraph with no markers.", bb(0.1, 0.1, 0.9, 0.2)),
            placed(&page_hk, run, 1, BlockKind::Footnote, "‡ only on the next page", bb(0.1, 0.9, 0.9, 0.94)),
        ];
        let mut d = doc_of(blocks, run, &doc);
        anchor_footnotes(&mut d);
        assert!(d.links.is_empty(), "anchoring is best-effort, not a guess");
    }

}
