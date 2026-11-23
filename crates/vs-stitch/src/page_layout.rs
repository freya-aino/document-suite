//! Intra-page structure: what a page's blocks are to *each other*.
//!
//! Before this, a page carried a single linear `reading_order` and nothing else. Every
//! relation the suite knew about was between pages — continuations, cross-references —
//! so a two-column paper, a form and a flat memo were structurally indistinguishable.
//!
//! Three things are derived here, all from geometry, all deterministic (no model calls,
//! matching the rest of this crate):
//!
//! - **Column flow**: multi-column reading order, repaired.
//! - **Enclosure**: one block geometrically inside another.
//! - **Key/value**: label-to-value pairs on forms, invoices and receipts.
//!
//! Coordinates are page-normalised `[0,1]` throughout, so every threshold below is a
//! fraction of the page and none of them depend on DPI.

use std::collections::HashMap;

use vs_core::{BBox, BlockKind, DocType};

use crate::{DocBlock, Link, LinkKind, StitchedDocument};

/// A block at least this wide is treated as spanning the page rather than sitting in a
/// column — a title, a full-width table, a banner. Getting this wrong is the classic
/// multi-column failure: a spanning title pulled into the left column drags everything
/// after it out of order.
const FULL_WIDTH: f32 = 0.6;

/// Horizontal gap between column groups, as a fraction of the page. Narrower than this
/// and they are more likely one ragged column than two.
const MIN_COLUMN_GAP: f32 = 0.02;

/// Fraction of the smaller block that must lie inside the larger to count as enclosed.
/// Not 1.0: a caption printed over the bottom edge of a figure is enclosed in intent
/// even when a few per cent of its box pokes out.
const ENCLOSURE_COVERAGE: f32 = 0.85;

/// A label and its value must overlap this much vertically (as a fraction of the shorter
/// one) to be read as being on the same line.
const SAME_LINE_OVERLAP: f32 = 0.5;

/// Longest a label may be, in characters. A paragraph ending in a colon is not a form
/// field, and this is the cheapest way to tell them apart.
const MAX_LABEL_CHARS: usize = 60;

fn width(b: &BBox) -> f32 {
    b.x1 - b.x0
}

fn height(b: &BBox) -> f32 {
    b.y1 - b.y0
}

/// Fraction of `inner` that lies inside `outer`.
fn coverage(inner: &BBox, outer: &BBox) -> f32 {
    let ix = (inner.x1.min(outer.x1) - inner.x0.max(outer.x0)).max(0.0);
    let iy = (inner.y1.min(outer.y1) - inner.y0.max(outer.y0)).max(0.0);
    let a = inner.area();
    if a <= 0.0 { 0.0 } else { (ix * iy) / a }
}

/// Fraction of the shorter box's height that the two share.
fn vertical_overlap(a: &BBox, b: &BBox) -> f32 {
    let overlap = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
    let shorter = height(a).min(height(b));
    if shorter <= 0.0 { 0.0 } else { overlap / shorter }
}

/// Horizontal overlap as a fraction of the narrower box's width.
pub(crate) fn horizontal_overlap(a: &BBox, b: &BBox) -> f32 {
    let overlap = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
    let narrower = width(a).min(width(b));
    if narrower <= 0.0 { 0.0 } else { overlap / narrower }
}

// ---------------------------------------------------------------------------------------
// Column flow
// ---------------------------------------------------------------------------------------

/// Reading order of one page's blocks, in column-major order within horizontal bands.
///
/// `boxes` are the page's blocks in the model's own order. The returned permutation is
/// the order they should be read in.
///
/// The algorithm, and why it has bands: a two-column paper is not simply two columns. It
/// is a spanning title, then two columns, then possibly a spanning figure, then two more
/// columns. Clustering the whole page into columns at once puts the title in the left
/// column and the figure after everything. So the page is first cut into horizontal bands
/// at every full-width block, and only the blocks between them are column-ordered.
pub fn column_order(boxes: &[BBox]) -> Vec<usize> {
    if boxes.len() < 2 {
        return (0..boxes.len()).collect();
    }
    // Bands are delimited by spanning blocks, which stay exactly where they are.
    let mut order = Vec::with_capacity(boxes.len());
    let mut band: Vec<usize> = Vec::new();
    let mut by_y: Vec<usize> = (0..boxes.len()).collect();
    by_y.sort_by(|a, b| boxes[*a].y0.partial_cmp(&boxes[*b].y0).unwrap_or(std::cmp::Ordering::Equal));

    for i in by_y {
        if width(&boxes[i]) >= FULL_WIDTH {
            order.extend(order_band(boxes, &band));
            band.clear();
            order.push(i);
        } else {
            band.push(i);
        }
    }
    order.extend(order_band(boxes, &band));
    order
}

/// Orders one band: left-to-right by column, top-to-bottom within each.
fn order_band(boxes: &[BBox], band: &[usize]) -> Vec<usize> {
    if band.len() < 2 {
        return band.to_vec();
    }
    let mut columns = columns_of(boxes, band);
    // Left to right by the column's left edge.
    columns.sort_by(|a, b| {
        let left = |c: &Vec<usize>| c.iter().map(|i| boxes[*i].x0).fold(f32::MAX, f32::min);
        left(a).partial_cmp(&left(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out = Vec::with_capacity(band.len());
    for mut col in columns {
        col.sort_by(|a, b| boxes[*a].y0.partial_cmp(&boxes[*b].y0).unwrap_or(std::cmp::Ordering::Equal));
        out.extend(col);
    }
    out
}

/// A gap narrower than `MIN_COLUMN_GAP` still separates two columns if **every** block in
/// the band stays on one side of it. This is how narrow the gap may be before even that
/// is not enough: below half a per cent of the page, a shared left or right edge is more
/// likely to be two blocks of one ragged column than a gutter.
const MIN_CONSISTENT_GAP: f32 = 0.005;

/// Groups a band's blocks into columns by horizontal overlap.
///
/// Single-pass sweep by left edge: a block joins the current column if it overlaps that
/// column's x-range, otherwise a gap of at least `MIN_COLUMN_GAP` starts a new one. Each
/// resulting group is then re-examined for a **consistent** narrower gutter — see
/// [`split_on_consistent_gap`], which is what keeps a tightly set two-column page from
/// reading as one column.
pub fn columns_of(boxes: &[BBox], band: &[usize]) -> Vec<Vec<usize>> {
    let mut by_x: Vec<usize> = band.to_vec();
    by_x.sort_by(|a, b| boxes[*a].x0.partial_cmp(&boxes[*b].x0).unwrap_or(std::cmp::Ordering::Equal));

    let mut columns: Vec<Vec<usize>> = Vec::new();
    let mut extent: Vec<(f32, f32)> = Vec::new();
    for i in by_x {
        let b = &boxes[i];
        match extent.last_mut() {
            // Overlaps the current column, or is closer to it than a column gap.
            Some((x0, x1)) if b.x0 <= *x1 + MIN_COLUMN_GAP => {
                *x0 = x0.min(b.x0);
                *x1 = x1.max(b.x1);
                columns.last_mut().expect("extent and columns stay in step").push(i);
            }
            _ => {
                extent.push((b.x0, b.x1));
                columns.push(vec![i]);
            }
        }
    }
    columns.into_iter().flat_map(|c| split_on_consistent_gap(boxes, c)).collect()
}

/// Splits a column group at a vertical gutter that no block crosses.
///
/// The sweep above needs a gap of `MIN_COLUMN_GAP` (2 % of the page) to start a new
/// column, which a tightly set two-column layout — a journal page, a densely laid-out
/// form — does not have, so the whole band came back as one column and its reading order
/// was repaired as if it were one. The fix does not need a smaller threshold, which would
/// split ragged single columns; it needs a different question: is there an x where
/// *everything* in this group is either entirely left of it or entirely right of it?
/// A gutter is exactly that, and a ragged column has no such x at all.
///
/// Recursive, so three tight columns split into three.
fn split_on_consistent_gap(boxes: &[BBox], group: Vec<usize>) -> Vec<Vec<usize>> {
    if group.len() < 2 {
        return vec![group];
    }
    // Candidate gutters: between consecutive right edges and the next left edge.
    let mut by_x = group.clone();
    by_x.sort_by(|a, b| boxes[*a].x0.partial_cmp(&boxes[*b].x0).unwrap_or(std::cmp::Ordering::Equal));
    let mut best: Option<(f32, f32)> = None; // (gap width, split x)
    for w in by_x.windows(2) {
        let left_of_split = boxes[w[0]].x1;
        let right_of_split = boxes[w[1]].x0;
        let gap = right_of_split - left_of_split;
        if gap < MIN_CONSISTENT_GAP {
            continue;
        }
        let split = (left_of_split + right_of_split) / 2.0;
        // Nothing may cross it. One block spanning the gutter means it is not one.
        if group.iter().any(|i| boxes[*i].x0 < split && boxes[*i].x1 > split) {
            continue;
        }
        // Both sides must hold something, and the widest clean gutter wins.
        if best.is_none_or(|(g, _)| gap > g) {
            best = Some((gap, split));
        }
    }
    let Some((_, split)) = best else { return vec![group] };
    let (left, right): (Vec<usize>, Vec<usize>) = group.into_iter().partition(|i| boxes[*i].x1 <= split);
    if left.is_empty() || right.is_empty() {
        return vec![left.into_iter().chain(right).collect()];
    }
    let mut out = split_on_consistent_gap(boxes, left);
    out.extend(split_on_consistent_gap(boxes, right));
    out
}

/// Repairs multi-column reading order in place, page by page, and records each block's
/// column.
///
/// Only *reorders* when it would change something **and** the page really looks like
/// several columns, because the model's own order is usually right and overriding it on a
/// single-column page could only make things worse. The column membership is recorded for
/// every multi-column page either way, including one whose order was already correct:
/// reordering is a repair, and "this page had two columns, and these blocks were in the
/// left one" is a fact about the page that a reader cannot recover afterwards — the repair
/// has by then made it look like any other linear page.
///
/// Returns the pages it reordered.
pub fn repair_reading_order(doc: &mut StitchedDocument) -> Vec<u32> {
    let mut by_page: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        by_page.entry(b.page_no).or_default().push(i);
    }
    let mut repaired = Vec::new();
    for (page_no, idx) in by_page {
        let boxes: Vec<BBox> = idx.iter().map(|i| doc.blocks[*i].block.bbox).collect();
        // A band with two or more columns is the only case worth touching.
        let narrow: Vec<usize> = (0..boxes.len()).filter(|i| width(&boxes[*i]) < FULL_WIDTH).collect();
        let columns = columns_of(&boxes, &narrow);
        let bands_have_columns = columns.len() >= 2;
        if !bands_have_columns {
            continue;
        }
        // Column membership, recorded whether or not the order needed repairing. A
        // spanning block is in no column and keeps `None`.
        let n_columns = columns.len().min(u8::MAX as usize) as u8;
        for (c, members) in columns.iter().enumerate() {
            for local in members {
                let b = &mut doc.blocks[idx[*local]];
                b.column = Some(c.min(u8::MAX as usize) as u8);
                b.page_columns = n_columns;
            }
        }
        // Every block on the page learns the page's shape, including the spanning ones
        // that belong to no column: `page_columns` is a fact about the page.
        for &i in &idx {
            doc.blocks[i].page_columns = n_columns;
        }
        let order = column_order(&boxes);
        let reordered: Vec<usize> = order.iter().map(|o| idx[*o]).collect();
        if reordered == idx {
            continue;
        }
        // Rewrite global_order to the repaired sequence, keeping each page's existing
        // span of order values so other pages are untouched.
        let mut orders: Vec<u32> = idx.iter().map(|i| doc.blocks[*i].global_order).collect();
        orders.sort_unstable();
        for (slot, block_idx) in orders.into_iter().zip(reordered) {
            doc.blocks[block_idx].global_order = slot;
        }
        repaired.push(page_no);
    }
    // `blocks` must stay sorted by global_order: everything downstream reads it in order.
    doc.blocks.sort_by_key(|b| b.global_order);
    repaired.sort_unstable();
    repaired
}

// ---------------------------------------------------------------------------------------
// Enclosure
// ---------------------------------------------------------------------------------------

/// `Encloses` links for blocks that sit geometrically inside another on the same page.
///
/// Most layout models emit sibling regions that do not overlap, so on a typical page this
/// finds nothing — and that is the correct answer. It matters where a model *does* nest:
/// a caption printed inside a figure's frame, list items inside a list region, a stamp
/// over a form. Each block is attributed to its **smallest** enclosing block, so a
/// caption inside a figure inside a column is linked to the figure, not the column.
pub fn enclosures(doc: &StitchedDocument) -> Vec<Link> {
    let mut by_page: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        by_page.entry(b.page_no).or_default().push(i);
    }
    let mut links = Vec::new();
    for idx in by_page.values() {
        for &inner in idx {
            let ib = &doc.blocks[inner].block.bbox;
            let mut best: Option<(usize, f32)> = None;
            for &outer in idx {
                if outer == inner {
                    continue;
                }
                let ob = &doc.blocks[outer].block.bbox;
                // Strictly larger, or two identical boxes would enclose each other.
                if ob.area() <= ib.area() {
                    continue;
                }
                if coverage(ib, ob) >= ENCLOSURE_COVERAGE {
                    let area = ob.area();
                    if best.is_none_or(|(_, a)| area < a) {
                        best = Some((outer, area));
                    }
                }
            }
            if let Some((outer, _)) = best {
                links.push(Link::new(
                    LinkKind::Encloses,
                    doc.blocks[outer].block.block_hk.clone(),
                    doc.blocks[inner].block.block_hk.clone(),
                    coverage(ib, &doc.blocks[outer].block.bbox).min(1.0),
                ));
            }
        }
    }
    links
}

// ---------------------------------------------------------------------------------------
// Key / value
// ---------------------------------------------------------------------------------------

/// A trailing colon: the one signal that a block is a label and **not** a value.
///
/// Worth its own predicate because the two questions are not the same question.
/// "Is this a label?" is a guess; "could this be somebody's value?" has to be answered
/// restrictively, or a form whose values happen to look label-shaped — `INV-2026-0042`,
/// `Acme Ltd` — pairs with nothing at all, which is what used to happen.
fn ends_with_a_colon(text: &str) -> bool {
    text.trim_end().ends_with([':', '：'])
}

/// Whether a block's text reads like a form label.
///
/// A trailing colon is the strongest signal and the one worth trusting; without it a
/// short run of title-case words is accepted, which is how most invoices label a field.
///
/// Digits rule it out when there is no colon, which is the rule that makes the whole
/// thing work on a real invoice: `INV-2026-0042`, `2026-03-01` and `1499.95` are all
/// short, all start with a character that is not lower-case, and are all values. A label
/// with a digit in it does exist (`Line 2 total`) but it is rarer than a value with one,
/// and the colon case still catches it.
pub fn looks_like_label(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > MAX_LABEL_CHARS {
        return false;
    }
    if ends_with_a_colon(t) {
        return true;
    }
    // No colon: require it to be short, and not a sentence.
    let words = t.split_whitespace().count();
    if words > 5 || t.ends_with(['.', '!', '?']) || t.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    // At least one word starting with an upper-case letter, which excludes stray values
    // like "n/a" being read as labels.
    t.split_whitespace().any(|w| w.chars().next().is_some_and(char::is_uppercase))
}

/// `ValueOf` links pairing each label with its value, on form-like pages only.
///
/// Gated on the **page's** type rather than the document's, so a mixed PDF — a contract
/// with an invoice stapled to the back — pairs fields only on the pages that are forms.
/// Running it everywhere would produce `ValueOf` links between every heading and the
/// paragraph under it.
///
/// A value is sought to the right on the same line first, then directly below, which is
/// the order the two layouts occur in: side-by-side fields are far more common, and
/// stacked ones only read as a pair when nothing sits to the right.
pub fn key_values(doc: &StitchedDocument, page_types: &HashMap<u32, DocType>) -> Vec<Link> {
    // The classifier's answer, **or** the page's own shape. Gating on the type alone
    // meant a form the classifier called `other` got no pairs at all — and a page of
    // short labelled fields is a form whatever anyone calls it, which is a thing geometry
    // can see for itself (`looks_like_a_form`).
    let structural: HashMap<u32, bool> = {
        let mut by_page: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, b) in doc.blocks.iter().enumerate() {
            if doc.is_body(i) {
                by_page.entry(b.page_no).or_default().push(i);
            }
        }
        by_page.into_iter().map(|(page, idx)| (page, looks_like_a_form(doc, &idx))).collect()
    };
    let form_like = |page: u32| {
        matches!(page_types.get(&page), Some(DocType::Form | DocType::Invoice | DocType::Receipt))
            || structural.get(&page).copied().unwrap_or(false)
    };
    let mut by_page: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        if form_like(b.page_no) && doc.is_body(i) && pairable(&doc.blocks[i]) {
            by_page.entry(b.page_no).or_default().push(i);
        }
    }

    let mut links = Vec::new();
    for idx in by_page.values() {
        let mut labels: Vec<usize> = idx.iter().copied().filter(|i| looks_like_label(&doc.blocks[*i].block.text)).collect();
        // Colon-terminated labels first, because they are the certain ones, and a block
        // they claim as a value is then not available to be read as a label itself. Two
        // title-case words beside `Customer:` are that customer's name; the same two
        // words with nothing to their left would be a label. Order is what tells them
        // apart, and without it `Acme Ltd` pairs with whatever sits under it.
        labels.sort_by_key(|i| !ends_with_a_colon(&doc.blocks[*i].block.text));
        let mut claimed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for &k in &labels {
            if claimed.contains(&k) {
                continue;
            }
            let kb = &doc.blocks[k].block.bbox;
            // Candidates to the right, on the same line.
            let right = idx
                .iter()
                .copied()
                .filter(|v| *v != k)
                .filter(|v| {
                    let vb = &doc.blocks[*v].block.bbox;
                    vb.x0 >= kb.x1 - 0.01 && vertical_overlap(kb, vb) >= SAME_LINE_OVERLAP
                })
                .min_by(|a, b| {
                    let d = |i: &usize| doc.blocks[*i].block.bbox.x0;
                    d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
                });
            // And directly below, in the same column.
            let below = idx
                .iter()
                .copied()
                .filter(|v| *v != k)
                .filter(|v| {
                    let vb = &doc.blocks[*v].block.bbox;
                    vb.y0 >= kb.y1 - 0.005 && horizontal_overlap(kb, vb) >= SAME_LINE_OVERLAP
                })
                .min_by(|a, b| {
                    let d = |i: &usize| doc.blocks[*i].block.bbox.y0;
                    d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
                });

            // Both, when both exist. A label with two values is not a malformed pair, it
            // is a shape that occurs: a date *range* under one label, an amount split
            // into net and gross, a field whose value continues on the next line. Keeping
            // only the first dropped the rest silently; the second is linked with a lower
            // confidence, which is the honest statement of how much weaker that reading
            // is.
            //
            // A label whose "value" is another label is a column header, not a pair —
            // but only a colon settles that. Rejecting everything label-*shaped* would
            // throw away `Customer: Acme Ltd`, where the value is two title-case words
            // and nothing about its shape says otherwise.
            let value_of = |v: usize| !ends_with_a_colon(&doc.blocks[v].block.text);
            for (candidate, confidence) in [(right, 0.8), (below, if right.is_some() { 0.45 } else { 0.6 })] {
                if let Some(v) = candidate.filter(|v| value_of(*v)) {
                    claimed.insert(v);
                    links.push(Link::new(
                        LinkKind::ValueOf,
                        doc.blocks[v].block.block_hk.clone(),
                        doc.blocks[k].block.block_hk.clone(),
                        confidence,
                    ));
                }
            }
        }
    }
    links
}

/// Whether a page's geometry reads like a form, whatever the classifier called it.
///
/// Two things together, because either alone is too easily satisfied: at least four
/// label-shaped blocks, and at least half of them having something to their right on the
/// same line. A page of prose has neither — a paragraph is too long to be a label — and a
/// table of contents has the first but not the second.
///
/// Deliberately conservative. A false positive produces `ValueOf` links between
/// unrelated blocks, which is worse than the absence this replaces, so the thresholds
/// here are set to miss sparse forms rather than to catch dense prose.
pub fn looks_like_a_form(doc: &StitchedDocument, page_blocks: &[usize]) -> bool {
    const MIN_FIELDS: usize = 4;
    let labels: Vec<usize> = page_blocks
        .iter()
        .copied()
        .filter(|i| pairable(&doc.blocks[*i]) && looks_like_label(&doc.blocks[*i].block.text))
        .collect();
    if labels.len() < MIN_FIELDS {
        return false;
    }
    let with_a_value = labels
        .iter()
        .filter(|k| {
            let kb = &doc.blocks[**k].block.bbox;
            page_blocks.iter().any(|v| {
                v != *k && {
                    let vb = &doc.blocks[*v].block.bbox;
                    vb.x0 >= kb.x1 - 0.01 && vertical_overlap(kb, vb) >= SAME_LINE_OVERLAP && !ends_with_a_colon(&doc.blocks[*v].block.text)
                }
            })
        })
        .count();
    with_a_value * 2 >= labels.len()
}

/// Kinds that can take part in a key/value pair. A table or figure is not a form field,
/// and pairing one would attach a whole table to a nearby label.
fn pairable(b: &DocBlock) -> bool {
    matches!(b.block.kind, BlockKind::Paragraph | BlockKind::Other | BlockKind::Heading | BlockKind::Handwriting)
        && !b.block.text.trim().is_empty()
}

// ---------------------------------------------------------------------------------------
// Heading level from type size
// ---------------------------------------------------------------------------------------

/// A heading's type must be this much larger than the page's body text before size is
/// read as a level at all. Below it, the difference is leading or a bold run, not a
/// hierarchy.
const MIN_HEADING_RATIO: f32 = 1.05;

/// Assigns a level to every heading that has none, from the type size the text layer
/// reports.
///
/// Numbering (`2.3.1 Results`) was the only fallback, so a document whose headings are
/// not numbered — most reports, most contracts, most web-sourced PDFs — had every heading
/// at level 1 and therefore a section tree one level deep. The type sizes are right
/// there in the text layer and say exactly what the numbering would have.
///
/// How it decides, and what it refuses to do: the **body** size is the size most of the
/// page's text is set in (the mode over spans, weighted by how much text each covers),
/// which is a far more robust reference than the largest or the mean. Heading sizes
/// *above* that are collected, rounded to half a point so that 13.98 and 14.02 are one
/// size, and ranked — the largest distinct size is level 1, the next level 2, and so on.
/// A heading no larger than body text keeps whatever it had, because at that point size
/// carries no information and a guess would be worse than the absence.
///
/// `spans` are `(bbox, font_size)` pairs in page-normalised coordinates: the type the
/// text layer carries. Taking pairs rather than the text-layer type keeps this crate from
/// depending on the artifact types, and the caller — the page stage, which is where the
/// text layer is loaded — does the mapping.
pub fn assign_heading_levels(blocks: &mut [vs_core::Block], spans: &[(BBox, f32)]) -> usize {
    let needs_level = blocks.iter().any(|b| b.kind == BlockKind::Heading && b.level.is_none());
    if !needs_level || spans.is_empty() {
        return 0;
    }
    let Some(body) = body_size(spans) else { return 0 };

    // Each heading's own size: the largest size covering it, because a heading's run is
    // usually one size and a stray smaller span (a footnote marker, a trailing label)
    // must not drag it down.
    let sizes: Vec<Option<f32>> = blocks
        .iter()
        .map(|b| {
            if b.kind != BlockKind::Heading {
                return None;
            }
            spans
                .iter()
                .filter(|(sb, _)| coverage(sb, &b.bbox) >= 0.5)
                .map(|(_, size)| *size)
                .filter(|s| *s > 0.0)
                .fold(None::<f32>, |acc, s| Some(acc.map_or(s, |a: f32| a.max(s))))
                .map(round_half)
        })
        .collect();

    // The distinct heading sizes above body text, largest first: rank is level.
    let mut ladder: Vec<f32> = sizes
        .iter()
        .flatten()
        .copied()
        .filter(|s| *s >= body * MIN_HEADING_RATIO)
        .collect();
    ladder.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    ladder.dedup();
    if ladder.is_empty() {
        return 0;
    }

    let mut assigned = 0;
    for (b, size) in blocks.iter_mut().zip(sizes) {
        if b.kind != BlockKind::Heading || b.level.is_some() {
            continue;
        }
        let Some(size) = size else { continue };
        if let Some(rank) = ladder.iter().position(|s| (*s - size).abs() < f32::EPSILON) {
            // Levels are 1-based and bounded at 6, as in HTML: a document with seven
            // distinct heading sizes is a document with a decorative one.
            b.level = Some((rank as u8 + 1).min(6));
            assigned += 1;
        }
    }
    assigned
}

/// The size most of the page's text is set in, weighted by how much of the page each span
/// covers.
///
/// Weighted by area rather than counted per span, because a page's spans are not equal:
/// one span can be a whole paragraph and the next a page number. Rounded to half a point
/// first, so the same body font does not spread across neighbouring buckets.
fn body_size(spans: &[(BBox, f32)]) -> Option<f32> {
    let mut weight: Vec<(f32, f32)> = Vec::new();
    for (bbox, size) in spans.iter().filter(|(_, s)| *s > 0.0) {
        let key = round_half(*size);
        let area = bbox.area().max(f32::MIN_POSITIVE);
        match weight.iter_mut().find(|(k, _)| (*k - key).abs() < f32::EPSILON) {
            Some((_, w)) => *w += area,
            None => weight.push((key, area)),
        }
    }
    weight.into_iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)).map(|(size, _)| size)
}

/// To the nearest half point, so 13.98 and 14.02 are the same size.
fn round_half(size: f32) -> f32 {
    (size * 2.0).round() / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb(x0: f32, y0: f32, x1: f32, y1: f32) -> BBox {
        BBox::new(x0, y0, x1, y1).unwrap()
    }

    #[test]
    fn single_column_is_left_alone() {
        let boxes = vec![bb(0.1, 0.1, 0.9, 0.2), bb(0.1, 0.3, 0.9, 0.4), bb(0.1, 0.5, 0.9, 0.6)];
        assert_eq!(column_order(&boxes), vec![0, 1, 2]);
    }

    /// The case this exists for: a model that read across the page instead of down a
    /// column, so the output interleaves the two.
    #[test]
    fn two_columns_are_read_down_then_across() {
        // Model order interleaves: L1, R1, L2, R2.
        let boxes = vec![
            bb(0.05, 0.10, 0.45, 0.20), // left 1
            bb(0.55, 0.10, 0.95, 0.20), // right 1
            bb(0.05, 0.25, 0.45, 0.35), // left 2
            bb(0.55, 0.25, 0.95, 0.35), // right 2
        ];
        assert_eq!(column_order(&boxes), vec![0, 2, 1, 3], "left column first, top to bottom");
    }

    /// A spanning title must not be pulled into a column.
    #[test]
    fn a_full_width_block_delimits_a_band() {
        let boxes = vec![
            bb(0.05, 0.02, 0.95, 0.08), // title, spans
            bb(0.05, 0.10, 0.45, 0.20), // left 1
            bb(0.55, 0.10, 0.95, 0.20), // right 1
            bb(0.05, 0.40, 0.95, 0.46), // spanning figure
            bb(0.05, 0.50, 0.45, 0.60), // left 2
            bb(0.55, 0.50, 0.95, 0.60), // right 2
        ];
        assert_eq!(
            column_order(&boxes),
            vec![0, 1, 2, 3, 4, 5],
            "bands keep spanning blocks in place and column-order only what is between them"
        );
    }

    #[test]
    fn three_columns_order_left_to_right() {
        let boxes = vec![
            bb(0.70, 0.10, 0.95, 0.20),
            bb(0.05, 0.10, 0.30, 0.20),
            bb(0.37, 0.10, 0.62, 0.20),
            bb(0.05, 0.30, 0.30, 0.40),
        ];
        assert_eq!(column_order(&boxes), vec![1, 3, 2, 0]);
    }

    #[test]
    fn a_ragged_single_column_is_not_two() {
        // Slightly different left edges and widths, but overlapping: one column.
        let boxes = vec![bb(0.10, 0.1, 0.55, 0.2), bb(0.12, 0.3, 0.58, 0.4), bb(0.10, 0.5, 0.50, 0.6)];
        let all: Vec<usize> = (0..boxes.len()).collect();
        assert_eq!(columns_of(&boxes, &all).len(), 1);
    }

    #[test]
    fn empty_and_single_inputs_are_handled() {
        assert!(column_order(&[]).is_empty());
        assert_eq!(column_order(&[bb(0.0, 0.0, 1.0, 1.0)]), vec![0]);
        assert!(columns_of(&[], &[]).is_empty());
    }

    #[test]
    fn coverage_and_overlap_helpers() {
        let outer = bb(0.0, 0.0, 1.0, 1.0);
        let inner = bb(0.2, 0.2, 0.4, 0.4);
        assert!((coverage(&inner, &outer) - 1.0).abs() < 1e-6);
        assert!((coverage(&outer, &inner) - 0.04).abs() < 1e-6, "got {}", coverage(&outer, &inner));
        // Half of the shorter box's height is shared.
        let a = bb(0.0, 0.0, 0.1, 0.2);
        let b = bb(0.2, 0.1, 0.3, 0.3);
        assert!((vertical_overlap(&a, &b) - 0.5).abs() < 1e-6);
        assert_eq!(horizontal_overlap(&a, &b), 0.0);
    }

    #[test]
    fn labels_need_a_colon_or_to_be_short_and_titled() {
        assert!(looks_like_label("Invoice number:"));
        assert!(looks_like_label("Total Due"));
        assert!(looks_like_label("Datum："), "full-width colon counts");
        assert!(!looks_like_label(""));
        assert!(!looks_like_label("12.00"), "a value is not a label");
        assert!(!looks_like_label("n/a"));
        assert!(
            !looks_like_label("The parties agree to the following terms and conditions set out below."),
            "a sentence is not a form field"
        );
        assert!(!looks_like_label("As set out in the schedule attached hereto."), "trailing stop rules it out");
        assert!(!looks_like_label(&"x".repeat(MAX_LABEL_CHARS + 1)));
    }
    /// Two columns with a 1 % gutter — tighter than `MIN_COLUMN_GAP`, which is what a
    /// journal page actually looks like. Before the consistent-gap split this came back
    /// as one column, and the page's reading order was then "repaired" as a single
    /// column, which is the wrong answer stated confidently.
    #[test]
    fn a_tight_gutter_is_still_two_columns() {
        let boxes = vec![
            bb(0.05, 0.10, 0.49, 0.20),
            bb(0.50, 0.10, 0.95, 0.20),
            bb(0.05, 0.25, 0.49, 0.35),
            bb(0.50, 0.25, 0.95, 0.35),
        ];
        let all: Vec<usize> = (0..boxes.len()).collect();
        assert_eq!(columns_of(&boxes, &all).len(), 2, "a 1 % gutter that nothing crosses is a gutter");
        assert_eq!(column_order(&boxes), vec![0, 2, 1, 3]);
    }

    #[test]
    fn a_block_crossing_the_gutter_means_there_is_no_gutter() {
        // The test that keeps the narrow-gap split honest: one block spanning the gap is
        // enough to say the page is not two columns there.
        let boxes = vec![bb(0.05, 0.10, 0.49, 0.20), bb(0.50, 0.10, 0.95, 0.20), bb(0.05, 0.25, 0.95, 0.35)];
        let all: Vec<usize> = (0..boxes.len()).collect();
        assert_eq!(columns_of(&boxes, &all).len(), 1);
    }

    #[test]
    fn three_tight_columns_split_into_three() {
        let boxes = vec![bb(0.02, 0.1, 0.31, 0.2), bb(0.33, 0.1, 0.64, 0.2), bb(0.66, 0.1, 0.98, 0.2)];
        let all: Vec<usize> = (0..boxes.len()).collect();
        assert_eq!(columns_of(&boxes, &all).len(), 3);
    }

    #[test]
    fn a_ragged_column_has_no_consistent_gutter() {
        // Indented quotes and a hanging first line: the shape that a smaller threshold
        // would have split and this does not.
        let boxes = vec![bb(0.10, 0.1, 0.55, 0.2), bb(0.14, 0.3, 0.58, 0.4), bb(0.10, 0.5, 0.50, 0.6)];
        let all: Vec<usize> = (0..boxes.len()).collect();
        assert_eq!(columns_of(&boxes, &all).len(), 1);
    }

    /// Repairing the order *erases* the shape of the page it corrected, so the column
    /// each block was in is recorded — otherwise a reader cannot tell a repaired
    /// two-column page from a single-column one, nor which blocks shared a column.
    #[test]
    fn a_repaired_page_remembers_that_it_had_columns() {
        let doc = vs_core::HashKey::of(&["d"]);
        let page_hk = vs_core::HashKey::page(&doc, 1);
        let run = vs_core::RunId::new();
        let place = |i: u32, bbox: BBox| {
            let mut b = heading(i, "text", bbox);
            b.kind = BlockKind::Paragraph;
            b.block_hk = vs_core::HashKey::block(&page_hk, &run, i);
            DocBlock { block: b, page_no: 1, page_hk: page_hk.clone(), global_order: i, column: None, page_columns: 0 }
        };
        // A spanning title, then two columns interleaved in the model's own order.
        let mut d = StitchedDocument {
            document_hk: doc,
            run_id: run,
            blocks: vec![
                place(0, bb(0.05, 0.02, 0.95, 0.08)), // spans
                place(1, bb(0.05, 0.10, 0.45, 0.20)), // left
                place(2, bb(0.55, 0.10, 0.95, 0.20)), // right
                place(3, bb(0.05, 0.25, 0.45, 0.35)), // left
                place(4, bb(0.55, 0.25, 0.95, 0.35)), // right
            ],
            furniture: Default::default(),
            sections: vec![],
            links: vec![],
            missing_pages: vec![],
        };
        let repaired = repair_reading_order(&mut d);
        assert_eq!(repaired, vec![1], "the page needed reordering");

        // Blocks are now in reading order, left column first.
        let columns: Vec<Option<u8>> = d.blocks.iter().map(|b| b.column).collect();
        assert_eq!(columns, vec![None, Some(0), Some(0), Some(1), Some(1)], "{columns:?}");
        assert!(d.blocks.iter().all(|b| b.page_columns == 2), "every block on the page knows the page had two columns");
        // The spanning title belongs to no column, which is the honest answer: it is not
        // in the left one merely because it starts there.
        assert_eq!(d.blocks[0].column, None);

        // A single-column page records nothing, so `page_columns == 0` means "not
        // multi-column" rather than "unknown".
        let mut plain = StitchedDocument {
            blocks: vec![place(0, bb(0.1, 0.1, 0.9, 0.2)), place(1, bb(0.1, 0.3, 0.9, 0.4))],
            ..d.clone()
        };
        assert!(repair_reading_order(&mut plain).is_empty());
        assert!(plain.blocks.iter().all(|b| b.column.is_none() && b.page_columns == 0));
    }

    // ───────────── heading levels from type size ─────────────

    fn heading(i: u32, text: &str, bbox: BBox) -> vs_core::Block {
        vs_core::Block {
            block_hk: vs_core::HashKey::of(&[&i.to_string()]),
            index: i,
            kind: BlockKind::Heading,
            bbox,
            reading_order: i,
            content: vs_core::BlockContent::Text { text: text.into() },
            text: text.into(),
            level: None,
            confidence: 0.9,
            provenance: vs_core::Provenance {
                run_id: vs_core::RunId::new(),
                dag_ref: "d@1".into(),
                node_id: "n".into(),
                step_kind: "s".into(),
                model_ref: None,
                prompt_ref: None,
                params_hash: "0".into(),
                started_at: chrono::Utc::now(),
                duration_ms: 1,
            },
        }
    }

    /// An unnumbered document: three heading sizes, each a level. Numbering was the only
    /// fallback before this, so every one of these was level 1 and the section tree came
    /// out flat.
    #[test]
    fn unnumbered_headings_get_their_level_from_type_size() {
        let mut blocks = vec![
            heading(0, "Introduction", bb(0.1, 0.10, 0.9, 0.14)),
            heading(1, "Method", bb(0.1, 0.30, 0.9, 0.33)),
            heading(2, "Participants", bb(0.1, 0.50, 0.9, 0.52)),
        ];
        let spans = vec![
            // Body text, by far the most of the page.
            (bb(0.1, 0.16, 0.9, 0.28), 10.0),
            (bb(0.1, 0.35, 0.9, 0.48), 10.0),
            (bb(0.1, 0.55, 0.9, 0.85), 10.0),
            // The headings themselves.
            (bb(0.1, 0.10, 0.9, 0.14), 18.0),
            (bb(0.1, 0.30, 0.9, 0.33), 14.0),
            (bb(0.1, 0.50, 0.9, 0.52), 12.0),
        ];
        assert_eq!(assign_heading_levels(&mut blocks, &spans), 3);
        assert_eq!(blocks.iter().map(|b| b.level).collect::<Vec<_>>(), vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_level_the_model_gave_is_never_overwritten() {
        let mut blocks = vec![heading(0, "Results", bb(0.1, 0.1, 0.9, 0.14))];
        blocks[0].level = Some(4);
        let spans = vec![(bb(0.1, 0.2, 0.9, 0.9), 10.0), (bb(0.1, 0.1, 0.9, 0.14), 20.0)];
        assert_eq!(assign_heading_levels(&mut blocks, &spans), 0);
        assert_eq!(blocks[0].level, Some(4), "a model that reported a level knows more than the type size");
    }

    #[test]
    fn a_heading_set_in_body_type_keeps_no_level_at_all() {
        // A run-in heading, bold but the same size: type size says nothing here, and a
        // guess would be worse than the absence.
        let mut blocks = vec![heading(0, "Discussion", bb(0.1, 0.1, 0.9, 0.12))];
        let spans = vec![(bb(0.1, 0.2, 0.9, 0.9), 10.0), (bb(0.1, 0.1, 0.9, 0.12), 10.0)];
        assert_eq!(assign_heading_levels(&mut blocks, &spans), 0);
        assert_eq!(blocks[0].level, None);
    }

    #[test]
    fn the_body_size_is_the_one_most_of_the_page_is_set_in() {
        // Weighted by area, not counted: one huge banner span must not become the body
        // size just by being one span among three.
        let spans = vec![(bb(0.1, 0.02, 0.9, 0.10), 30.0), (bb(0.1, 0.15, 0.9, 0.95), 10.0), (bb(0.1, 0.96, 0.3, 0.98), 8.0)];
        assert_eq!(body_size(&spans), Some(10.0));
        // Near-identical sizes are one size, so a body font does not spread across buckets.
        let jittered = vec![(bb(0.1, 0.1, 0.9, 0.5), 9.98), (bb(0.1, 0.5, 0.9, 0.9), 10.02)];
        assert_eq!(body_size(&jittered), Some(10.0));
    }

    #[test]
    fn with_no_type_sizes_nothing_is_assigned() {
        let mut blocks = vec![heading(0, "Introduction", bb(0.1, 0.1, 0.9, 0.14))];
        assert_eq!(assign_heading_levels(&mut blocks, &[]), 0);
        assert_eq!(blocks[0].level, None);
    }

    // ───────────── key/value ─────────────

    fn kv_doc(blocks: Vec<(BlockKind, &str, BBox)>) -> StitchedDocument {
        let doc = vs_core::HashKey::of(&["d"]);
        let page_hk = vs_core::HashKey::page(&doc, 1);
        let run = vs_core::RunId::new();
        let docblocks = blocks
            .into_iter()
            .enumerate()
            .map(|(i, (kind, text, bbox))| {
                let mut b = heading(i as u32, text, bbox);
                b.kind = kind;
                b.block_hk = vs_core::HashKey::block(&page_hk, &run, i as u32);
                DocBlock { block: b, page_no: 1, page_hk: page_hk.clone(), global_order: i as u32, column: None, page_columns: 0 }
            })
            .collect();
        StitchedDocument {
            document_hk: doc,
            run_id: run,
            blocks: docblocks,
            furniture: Default::default(),
            sections: vec![],
            links: vec![],
            missing_pages: vec![],
        }
    }

    /// Four labelled fields with values beside them: a form, whatever the classifier
    /// called the page. Gating on the classifier alone meant a misclassified form got no
    /// pairs at all.
    #[test]
    fn a_page_shaped_like_a_form_is_paired_even_when_it_was_classified_as_other() {
        let doc = kv_doc(vec![
            (BlockKind::Paragraph, "Invoice number:", bb(0.1, 0.10, 0.3, 0.13)),
            (BlockKind::Paragraph, "INV-2026-0042", bb(0.35, 0.10, 0.6, 0.13)),
            (BlockKind::Paragraph, "Date:", bb(0.1, 0.16, 0.3, 0.19)),
            (BlockKind::Paragraph, "2026-03-01", bb(0.35, 0.16, 0.6, 0.19)),
            (BlockKind::Paragraph, "Customer:", bb(0.1, 0.22, 0.3, 0.25)),
            (BlockKind::Paragraph, "Acme Ltd", bb(0.35, 0.22, 0.6, 0.25)),
            (BlockKind::Paragraph, "Total:", bb(0.1, 0.28, 0.3, 0.31)),
            (BlockKind::Paragraph, "1499.95", bb(0.35, 0.28, 0.6, 0.31)),
        ]);
        let page_blocks: Vec<usize> = (0..doc.blocks.len()).collect();
        assert!(looks_like_a_form(&doc, &page_blocks));
        // No entry in `page_types` at all: the classifier said nothing useful.
        let links = key_values(&doc, &HashMap::new());
        assert_eq!(links.len(), 4, "{links:?}");
        assert!(links.iter().all(|l| l.kind == LinkKind::ValueOf));
    }

    #[test]
    fn prose_is_not_a_form_however_many_colons_it_has() {
        let doc = kv_doc(vec![
            (BlockKind::Paragraph, "The parties agree as follows:", bb(0.1, 0.10, 0.9, 0.16)),
            (
                BlockKind::Paragraph,
                "Each party shall bear its own costs in connection with the negotiation of this agreement.",
                bb(0.1, 0.18, 0.9, 0.30),
            ),
            (BlockKind::Paragraph, "Notice:", bb(0.1, 0.32, 0.9, 0.36)),
            (
                BlockKind::Paragraph,
                "Any notice given under this agreement shall be in writing and delivered by hand.",
                bb(0.1, 0.38, 0.9, 0.50),
            ),
        ]);
        let page_blocks: Vec<usize> = (0..doc.blocks.len()).collect();
        assert!(!looks_like_a_form(&doc, &page_blocks), "a contract read as a form would pair clause for clause");
        assert!(key_values(&doc, &HashMap::new()).is_empty());
    }

    /// A label with a value beside it *and* under it: a date range, a split amount, a
    /// value that wrapped. Keeping only the first dropped the rest silently.
    #[test]
    fn a_label_with_two_values_keeps_both_and_says_which_is_weaker() {
        let doc = kv_doc(vec![
            (BlockKind::Paragraph, "Period:", bb(0.1, 0.10, 0.25, 0.13)),
            (BlockKind::Paragraph, "2026-01-01", bb(0.30, 0.10, 0.5, 0.13)),
            (BlockKind::Paragraph, "2026-12-31", bb(0.10, 0.14, 0.25, 0.17)),
        ]);
        let mut types = HashMap::new();
        types.insert(1u32, DocType::Invoice);
        let links = key_values(&doc, &types);
        assert_eq!(links.len(), 2, "{links:?}");
        let side_by_side = links.iter().find(|l| l.confidence > 0.7).expect("the value on the same line");
        let below = links.iter().find(|l| l.confidence < 0.7).expect("the value underneath");
        assert_ne!(side_by_side.from, below.from);
    }

}
