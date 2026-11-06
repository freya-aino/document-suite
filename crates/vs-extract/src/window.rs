//! Section-scoped extraction windows.

use std::collections::HashSet;

use vs_stitch::{LinkKind, StitchedDocument};

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

/// Packs body blocks into windows of at most `max_chars`, per section, never splitting a
/// block and keeping `ContinuesOn` chains together. A single block larger than `max_chars`
/// becomes its own window.
pub fn build_windows(doc: &StitchedDocument, max_chars: usize) -> Vec<Window> {
    // Blocks in sections, plus body blocks before the first heading as a pseudo-section.
    let in_sections: HashSet<usize> = doc.sections.iter().flat_map(|s| s.body.iter().copied()).collect();
    let mut groups: Vec<(Vec<String>, Vec<usize>)> = vec![];
    let preamble: Vec<usize> = (0..doc.blocks.len())
        .filter(|&i| doc.is_body(i) && !in_sections.contains(&i) && !doc.sections.iter().any(|s| s.heading == doc.blocks[i].block.block_hk))
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
        groups.push((path, s.body.clone()));
    }

    let continues: HashSet<(&str, &str)> = doc
        .links
        .iter()
        .filter(|l| l.kind == LinkKind::ContinuesOn)
        .map(|l| (l.from.as_str(), l.to.as_str()))
        .collect();

    let mut windows = vec![];
    for (path, body) in groups {
        let mut cur = Window { section_path: path.clone(), blocks: vec![], context: vec![], chars: 0 };
        for (k, &i) in body.iter().enumerate() {
            let len = doc.blocks[i].block.text.chars().count();
            let chained = k > 0 && continues.contains(&(doc.blocks[body[k - 1]].block.block_hk.as_str(), doc.blocks[i].block.block_hk.as_str()));
            if !cur.blocks.is_empty() && cur.chars + len > max_chars && !chained {
                windows.push(std::mem::replace(&mut cur, Window { section_path: path.clone(), blocks: vec![], context: vec![], chars: 0 }));
            }
            cur.blocks.push(i);
            cur.chars += len;
        }
        if !cur.blocks.is_empty() {
            windows.push(cur);
        }
    }

    // Context: captions and their targets linked to blocks referred to from inside the window.
    for w in &mut windows {
        let inside: HashSet<&str> = w.blocks.iter().map(|&i| doc.blocks[i].block.block_hk.as_str()).collect();
        let mut ctx: Vec<usize> = doc
            .links
            .iter()
            .filter(|l| l.kind == LinkKind::RefersTo && inside.contains(l.from.as_str()))
            .flat_map(|l| {
                let target = l.to.as_str();
                doc.links
                    .iter()
                    .filter(move |c| c.kind == LinkKind::CaptionOf && c.to.as_str() == target)
                    .map(|c| c.from.as_str())
                    .chain(std::iter::once(target))
            })
            .filter_map(|hk| doc.blocks.iter().position(|b| b.block.block_hk.as_str() == hk))
            .filter(|i| !w.blocks.contains(i))
            .collect();
        ctx.sort_unstable();
        ctx.dedup();
        w.context = ctx;
    }
    windows
}
