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
fn horizontal_overlap(a: &BBox, b: &BBox) -> f32 {
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

/// Groups a band's blocks into columns by horizontal overlap.
///
/// Single-pass sweep by left edge: a block joins the current column if it overlaps that
/// column's x-range, otherwise a gap of at least `MIN_COLUMN_GAP` starts a new one.
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
    columns
}

/// Repairs multi-column reading order in place, page by page.
///
/// Only acts when it would actually change something *and* the page really looks like
/// several columns, because the model's own order is usually right and overriding it on a
/// single-column page could only make things worse. Returns the pages it reordered.
pub fn repair_reading_order(doc: &mut StitchedDocument) -> Vec<u32> {
    let mut by_page: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        by_page.entry(b.page_no).or_default().push(i);
    }
    let mut repaired = Vec::new();
    for (page_no, idx) in by_page {
        let boxes: Vec<BBox> = idx.iter().map(|i| doc.blocks[*i].block.bbox).collect();
        // A band with two or more columns is the only case worth touching.
        let bands_have_columns = {
            let narrow: Vec<usize> = (0..boxes.len()).filter(|i| width(&boxes[*i]) < FULL_WIDTH).collect();
            columns_of(&boxes, &narrow).len() >= 2
        };
        if !bands_have_columns {
            continue;
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

/// Whether a block's text reads like a form label.
///
/// A trailing colon is the strongest signal and the one worth trusting; without it a
/// short run of title-case words is accepted, which is how most invoices label a field.
pub fn looks_like_label(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > MAX_LABEL_CHARS {
        return false;
    }
    if t.ends_with([':', '：']) {
        return true;
    }
    // No colon: require it to be short, and not a sentence.
    let words = t.split_whitespace().count();
    if words > 5 || t.ends_with(['.', '!', '?']) {
        return false;
    }
    // At least one word starting with an upper-case letter, which excludes stray values
    // like "12.00" or "n/a" being read as labels.
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
    let form_like = |page: u32| matches!(page_types.get(&page), Some(DocType::Form | DocType::Invoice | DocType::Receipt));
    let mut by_page: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        if form_like(b.page_no) && doc.is_body(i) && pairable(&doc.blocks[i]) {
            by_page.entry(b.page_no).or_default().push(i);
        }
    }

    let mut links = Vec::new();
    for idx in by_page.values() {
        let labels: Vec<usize> = idx.iter().copied().filter(|i| looks_like_label(&doc.blocks[*i].block.text)).collect();
        for &k in &labels {
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
            // Otherwise directly below, in the same column.
            let candidate = right.or_else(|| {
                idx.iter()
                    .copied()
                    .filter(|v| *v != k)
                    .filter(|v| {
                        let vb = &doc.blocks[*v].block.bbox;
                        vb.y0 >= kb.y1 - 0.005 && horizontal_overlap(kb, vb) >= SAME_LINE_OVERLAP
                    })
                    .min_by(|a, b| {
                        let d = |i: &usize| doc.blocks[*i].block.bbox.y0;
                        d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
                    })
            });
            // A label whose "value" is another label is a column header, not a pair.
            if let Some(v) = candidate.filter(|v| !looks_like_label(&doc.blocks[*v].block.text)) {
                let same_line = doc.blocks[v].block.bbox.x0 >= kb.x1 - 0.01;
                links.push(Link::new(
                    LinkKind::ValueOf,
                    doc.blocks[v].block.block_hk.clone(),
                    doc.blocks[k].block.block_hk.clone(),
                    // Side-by-side is the more reliable reading of the two.
                    if same_line { 0.8 } else { 0.6 },
                ));
            }
        }
    }
    links
}

/// Kinds that can take part in a key/value pair. A table or figure is not a form field,
/// and pairing one would attach a whole table to a nearby label.
fn pairable(b: &DocBlock) -> bool {
    matches!(b.block.kind, BlockKind::Paragraph | BlockKind::Other | BlockKind::Heading | BlockKind::Handwriting)
        && !b.block.text.trim().is_empty()
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
}
