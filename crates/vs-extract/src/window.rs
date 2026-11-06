//! Section-scoped extraction windows and their prompt rendering.

use std::collections::{HashMap, HashSet};

use vs_core::{BlockKind, HashKey};
use vs_stitch::{LinkKind, StitchedDocument};

use crate::bib::reference_block_indices;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Section path, e.g. ["3 Experiments", "3.2 Results"].
    pub section_path: Vec<String>,
    /// Indices into `StitchedDocument::blocks`, in reading order.
    pub blocks: Vec<usize>,
    /// Context blocks (captions, figure descriptions, tables referred to from the window).
    pub context: Vec<usize>,
    pub chars: usize,
}

/// A window rendered for the prompt: header, body and the `[B<n>]` label map.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedWindow {
    /// Document title, section path and referenced material (also labelled).
    pub header: String,
    /// `[B<n>] text` per body block, blank-line separated.
    pub body: String,
    /// Label (`B1`, …) → block hash key. Body blocks first, then context blocks.
    pub labels: HashMap<String, HashKey>,
}

impl RenderedWindow {
    /// Full prompt text: header, then body.
    pub fn text(&self) -> String {
        format!("{}\n\n{}", self.header, self.body)
    }
}

/// Block kinds that carry extractable running text. Tables are handled by `tables`, reference
/// lists by `bib`; both still appear as window *context* when referred to.
fn is_text_block(kind: BlockKind) -> bool {
    !matches!(
        kind,
        BlockKind::Table
            | BlockKind::Formula
            | BlockKind::Reference
            | BlockKind::Heading
            | BlockKind::Title
            | BlockKind::Header
            | BlockKind::Footer
            | BlockKind::PageNumber
    )
}

/// Title of the document: the `Title` block, else the first level-0 section, else `None`.
pub fn document_title(doc: &StitchedDocument) -> Option<String> {
    doc.blocks
        .iter()
        .find(|b| b.block.kind == BlockKind::Title && !doc.furniture.contains(&b.block.block_hk))
        .map(|b| b.block.text.trim().to_string())
        .or_else(|| doc.sections.iter().find(|s| s.level == 0).map(|s| s.title.trim().to_string()))
        .filter(|t| !t.is_empty())
}

/// Packs body text blocks into windows of at most `max_chars`, per section, never splitting a
/// block and keeping `ContinuesOn` chains together. A single block larger than `max_chars`
/// becomes its own window. Tables, formulas and reference-list blocks are not windowed (they
/// are handled by dedicated extractors) but appear as context when referred to.
pub fn build_windows(doc: &StitchedDocument, max_chars: usize) -> Vec<Window> {
    let refs: HashSet<usize> = reference_block_indices(doc).into_iter().collect();
    let eligible = |i: usize| doc.is_body(i) && is_text_block(doc.blocks[i].block.kind) && !refs.contains(&i) && !doc.blocks[i].block.text.trim().is_empty();

    // Blocks in sections, plus body blocks before the first heading as a pseudo-section.
    let in_sections: HashSet<usize> = doc.sections.iter().flat_map(|s| s.body.iter().copied()).collect();
    let headings: HashSet<&HashKey> = doc.sections.iter().map(|s| &s.heading).collect();
    let mut groups: Vec<(Vec<String>, Vec<usize>)> = vec![];
    let preamble: Vec<usize> = (0..doc.blocks.len())
        .filter(|&i| eligible(i) && !in_sections.contains(&i) && !headings.contains(&doc.blocks[i].block.block_hk))
        .collect();
    if !preamble.is_empty() {
        groups.push((vec![], preamble));
    }
    for s in &doc.sections {
        let mut path = vec![s.title.clone()];
        let mut p = s.parent;
        while let Some(pi) = p {
            path.insert(0, doc.sections[pi].title.clone());
            p = doc.sections[pi].parent;
        }
        let body: Vec<usize> = s.body.iter().copied().filter(|&i| eligible(i)).collect();
        if !body.is_empty() {
            groups.push((path, body));
        }
    }

    let continues: HashSet<(&str, &str)> = doc
        .links
        .iter()
        .filter(|l| l.kind == LinkKind::ContinuesOn)
        .map(|l| (l.from.as_str(), l.to.as_str()))
        .collect();

    let mut windows = vec![];
    for (path, body) in groups {
        let fresh = || Window { section_path: path.clone(), blocks: vec![], context: vec![], chars: 0 };
        let mut cur = fresh();
        for (k, &i) in body.iter().enumerate() {
            let len = doc.blocks[i].block.text.chars().count();
            let chained = k > 0 && continues.contains(&(doc.blocks[body[k - 1]].block.block_hk.as_str(), doc.blocks[i].block.block_hk.as_str()));
            if !cur.blocks.is_empty() && cur.chars + len > max_chars && !chained {
                windows.push(std::mem::replace(&mut cur, fresh()));
            }
            cur.blocks.push(i);
            cur.chars += len;
        }
        if !cur.blocks.is_empty() {
            windows.push(cur);
        }
    }

    // Context: targets referred to from inside the window, plus their captions; and, when a
    // caption itself is inside the window, the captioned figure/table.
    let position: HashMap<&str, usize> = doc.blocks.iter().enumerate().map(|(i, b)| (b.block.block_hk.as_str(), i)).collect();
    for w in &mut windows {
        let inside: HashSet<&str> = w.blocks.iter().map(|&i| doc.blocks[i].block.block_hk.as_str()).collect();
        let mut targets: Vec<&str> = doc
            .links
            .iter()
            .filter(|l| inside.contains(l.from.as_str()) && matches!(l.kind, LinkKind::RefersTo | LinkKind::CaptionOf))
            .map(|l| l.to.as_str())
            .collect();
        let captions: Vec<&str> = doc
            .links
            .iter()
            .filter(|c| c.kind == LinkKind::CaptionOf && targets.contains(&c.to.as_str()))
            .map(|c| c.from.as_str())
            .collect();
        targets.extend(captions);
        let mut ctx: Vec<usize> = targets
            .into_iter()
            .filter_map(|hk| position.get(hk).copied())
            .filter(|i| !w.blocks.contains(i) && !doc.blocks[*i].block.text.trim().is_empty())
            .collect();
        ctx.sort_unstable();
        ctx.dedup();
        w.context = ctx;
    }
    windows
}

/// Renders a window for the extraction prompt. Labels are `B1..Bn` for body blocks, continuing
/// for context blocks.
pub fn render_window(doc: &StitchedDocument, w: &Window, title: Option<&str>) -> RenderedWindow {
    let mut labels = HashMap::new();
    let mut header = String::new();
    header.push_str(&format!("Document: {}\n", title.unwrap_or("(untitled)")));
    header.push_str(&format!("Section: {}\n", if w.section_path.is_empty() { "(front matter)".to_string() } else { w.section_path.join(" > ") }));

    let mut body = String::new();
    let mut n = 0usize;
    for &i in &w.blocks {
        n += 1;
        let label = format!("B{n}");
        labels.insert(label.clone(), doc.blocks[i].block.block_hk.clone());
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&format!("[{label}] {}", doc.blocks[i].block.text.trim()));
    }
    if !w.context.is_empty() {
        header.push_str("\nReferenced material:\n");
        for &i in &w.context {
            n += 1;
            let label = format!("B{n}");
            let b = &doc.blocks[i].block;
            labels.insert(label.clone(), b.block_hk.clone());
            header.push_str(&format!("[{label}] ({}) {}\n", kind_name(b.kind), b.text.trim()));
        }
    }
    RenderedWindow { header: header.trim_end().to_string(), body, labels }
}

fn kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Table => "table",
        BlockKind::Figure => "figure",
        BlockKind::Chart => "chart",
        BlockKind::Caption => "caption",
        BlockKind::Formula => "formula",
        BlockKind::Footnote => "footnote",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::DocBuilder;
    use vs_core::BlockContent;

    #[test]
    fn groups_by_section_and_packs() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Title, "A Paper");
        b.block(BlockKind::Paragraph, "Abstract text before any heading."); // belongs to title section (level 0)
        b.block(BlockKind::Heading, "1 Introduction");
        b.block(BlockKind::Paragraph, &"a".repeat(60));
        b.block(BlockKind::Paragraph, &"b".repeat(60));
        b.block(BlockKind::Paragraph, &"c".repeat(60));
        b.block(BlockKind::Heading, "2 Method");
        b.block(BlockKind::Paragraph, "method text");
        b.block(BlockKind::Table, "t");
        let ws = build_windows(&b.doc, 130);
        // title section: 1 window; intro: 3 blocks of 60 chars → [a,b],[c]; method: 1 window (table excluded)
        assert_eq!(ws.len(), 4);
        assert_eq!(ws[0].section_path, vec!["A Paper".to_string()]);
        assert_eq!(ws[1].section_path, vec!["A Paper".to_string(), "1 Introduction".to_string()]);
        assert_eq!(ws[1].blocks, vec![3, 4]);
        assert_eq!(ws[2].blocks, vec![5]);
        assert_eq!(ws[3].blocks, vec![7]);
        assert_eq!(document_title(&b.doc).as_deref(), Some("A Paper"));
    }

    #[test]
    fn continues_on_keeps_blocks_together_and_oversized_block_is_own_window() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Heading, "1 Intro");
        let x = b.block(BlockKind::Paragraph, &"x".repeat(80));
        let y = b.block(BlockKind::Paragraph, &"y".repeat(80));
        b.block(BlockKind::Paragraph, &"z".repeat(500));
        b.link(LinkKind::ContinuesOn, x, y);
        let ws = build_windows(&b.doc, 100);
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[0].blocks, vec![x, y]);
        assert_eq!(ws[1].blocks, vec![3]);
    }

    #[test]
    fn preamble_without_heading_and_furniture_skipped() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Paragraph, "Loose text.");
        let f = b.block(BlockKind::Paragraph, "Running header");
        b.doc.furniture.insert(b.hk(f));
        let ws = build_windows(&b.doc, 1000);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].section_path.is_empty());
        assert_eq!(ws[0].blocks, vec![0]);
    }

    #[test]
    fn context_and_rendering() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Title, "Paper");
        b.block(BlockKind::Heading, "3 Results");
        let p = b.block(BlockKind::Paragraph, "Table 1 shows that BERT wins.");
        let t = b.block_with(BlockKind::Table, "Model Acc\nBERT 92.1", BlockContent::Table { html: "<table></table>".into() });
        let c = b.block(BlockKind::Caption, "Table 1: Results on GLUE.");
        b.link(LinkKind::RefersTo, p, t);
        b.link(LinkKind::CaptionOf, c, t);
        let ws = build_windows(&b.doc, 1000);
        let w = ws.iter().find(|w| w.blocks.contains(&p)).unwrap();
        // the caption is itself a text block in the same section, so only the table is context
        assert_eq!(w.blocks, vec![p, c]);
        assert_eq!(w.context, vec![t]);
        let r = render_window(&b.doc, w, Some("Paper"));
        assert!(r.header.starts_with("Document: Paper\nSection: Paper > 3 Results"));
        assert!(r.header.contains("[B3] (table) Model Acc"));
        assert_eq!(r.body, "[B1] Table 1 shows that BERT wins.\n\n[B2] Table 1: Results on GLUE.");
        assert_eq!(r.labels["B1"], b.hk(p));
        assert_eq!(r.labels["B3"], b.hk(t));
        assert_eq!(r.labels.len(), 3);
    }
}
