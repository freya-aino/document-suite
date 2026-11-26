//! A ground-truthed corpus, generated.
//!
//! `ds-adapt`'s status said it plainly: *"No dataset exists in-repo or is referenced; the
//! manifest format is specified but there is nothing to point it at."* Every metric here
//! was unit-tested against synthetic inputs and the five commands had never run as a
//! chain, because running them needs documents with labels and there were none.
//!
//! This generates them: small PDFs written here, page by page, with the ground truth
//! emitted from **the same geometry the page was written with**. That makes the labels
//! mechanically exact rather than annotated — a property no human-labelled corpus has,
//! and the reason this is worth having even though it is synthetic.
//!
//! **What it is not.** These pages are born-digital, clean, in one typeface, with no
//! scanner noise, no skew, no handwriting and no photographs. A DAG that wins here has
//! been shown to win on *this*, which is a statement about the machinery — that
//! `generate → run → score → select → report` produces a routed DAG list from labelled
//! documents — and not about document parsing. Calibrating extraction quality still needs
//! real documents and real models; `SPEC.md` keeps that distinction.
//!
//! The PDF is written by hand rather than with a crate: a page of text boxes at known
//! positions is a few hundred bytes of PDF operators, and pulling in a PDF *writer* to
//! produce input for a PDF *reader* would make the fixture depend on a second
//! interpretation of the format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use ds_core::DocType;

/// A4 at 72 dpi, which is the PDF user-space unit.
const PAGE_W: f32 = 595.0;
const PAGE_H: f32 = 842.0;
/// Helvetica's average advance width, as a fraction of the point size.
///
/// The ground-truth box for a line is derived from this, so it is the one approximation
/// in an otherwise exact label: a proportional font's real width needs the font's own
/// metrics. It is close enough that a box matches at IoU 0.5 — which is the threshold
/// `metrics::layout_f1` uses — and far too crude to score a layout model at IoU 0.9, so
/// nothing here claims to.
const AVG_ADVANCE: f32 = 0.5;

/// One line of text on a page, with the block kind it belongs to.
struct Line {
    text: String,
    size: f32,
    /// Baseline position in PDF user space (origin bottom-left).
    x: f32,
    y: f32,
}

impl Line {
    /// The line's bounding box in PDF user space, from the baseline and the type size.
    ///
    /// Ascent and descent are Helvetica's nominal 0.75 / 0.21 of the size, which is what
    /// makes this a *mechanical* label: the same numbers that placed the text compute the
    /// box.
    fn bbox(&self) -> [f32; 4] {
        let w = self.text.chars().count() as f32 * self.size * AVG_ADVANCE;
        [self.x, self.y - 0.21 * self.size, self.x + w, self.y + 0.75 * self.size]
    }
}

/// A logical block: the unit a parser is asked for, and so the unit the ground truth is
/// scored at.
///
/// The lines are what was drawn; the block's box is their hull. The distinction is the
/// whole point of this type. A page parser returns *paragraphs*, not the lines a
/// paragraph was typeset into, so a per-line label scores a correct reading as a total
/// miss: four line boxes against one predicted paragraph box is IoU ~0.25 against each,
/// no match at the 0.5 threshold, `layout_f1` and the order score both 0, and the
/// composite then falls below `Objective::quality_floor` for every candidate. The lines
/// stay in the ground truth as a nested field, for a reader that wants them.
struct Block {
    kind: &'static str,
    lines: Vec<Line>,
    /// Cell texts by row, header first, when this block is a table.
    ///
    /// Without it the ground truth carries no `html`, `score::labelled_metrics` falls
    /// back to cell-text similarity, and `metrics::teds` — the actual table-structure
    /// metric — never runs on this corpus at all.
    grid: Option<Vec<Vec<String>>>,
}

impl Block {
    /// One line per block is the common case; several are joined with a space, which is
    /// what `metrics::normalise_ws` would do to them anyway.
    fn text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// The hull of the lines' boxes.
    fn bbox(&self) -> [f32; 4] {
        let mut it = self.lines.iter().map(|l| l.bbox());
        let first = it.next().unwrap_or([0.0; 4]);
        it.fold(first, |a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])
    }

    /// `<table>` markup for a grid block, in the subset `metrics::parse_table` reads.
    ///
    /// Header cells are written as `td` rather than `th` because the parser normalises
    /// `th` to `td`, so emitting `th` would only invite a difference that scores as a
    /// relabel against a model that happens to choose the other one.
    fn html(&self) -> Option<String> {
        let grid = self.grid.as_ref()?;
        let mut out = String::from("<table>");
        for row in grid {
            out.push_str("<tr>");
            for cell in row {
                out.push_str("<td>");
                for c in cell.chars() {
                    match c {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        _ => out.push(c),
                    }
                }
                out.push_str("</td>");
            }
            out.push_str("</tr>");
        }
        out.push_str("</table>");
        Some(out)
    }
}

/// What kind of page to generate. Each shape exercises a different routing class, which
/// is what makes a *selection* over them meaningful rather than a single-class average.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageShape {
    /// Title, headings, paragraphs: one column.
    Prose,
    /// Two columns of paragraphs under a spanning title — the shape column repair exists
    /// for.
    TwoColumn,
    /// A labelled field grid: the form case, and the one `looks_like_a_form` reads.
    Form,
    /// A heading, a ruled table of numbers, and its caption.
    Table,
}

impl PageShape {
    fn doc_type(self) -> DocType {
        match self {
            PageShape::Prose => DocType::Report,
            PageShape::TwoColumn => DocType::ScientificPaper,
            PageShape::Form => DocType::Form,
            PageShape::Table => DocType::ScientificPaper,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PageShape::Prose => "prose",
            PageShape::TwoColumn => "two_column",
            PageShape::Form => "form",
            PageShape::Table => "table",
        }
    }
}

/// How a page is perturbed before it is scored against.
///
/// Not training data — nothing here trains, and `docs/SPEC.md` lists training as a
/// non-goal. These are **evaluation inputs**, and they exist because the per-page
/// conditioner steps that already ship — `page.orient`, `page.deskew`, `page.denoise`,
/// `page.normalize`, `page.binarize`, `page.crop_margins` — have nothing to correct on a
/// clean upright page. Three templates in `config/search-space.example.json` are built on
/// them, and against this corpus they could only ever lose: a conditioner that repairs
/// nothing is pure added latency, so the search's one available conclusion was "skip the
/// repair steps on pristine input". True, trivial, and easily misread as a finding about
/// document parsing.
///
/// Every transform here is applied **analytically to the labels as well**, so the ground
/// truth stays mechanically exact. That is the whole constraint: a perturbation whose
/// labels would have to be re-annotated does not belong in this generator, which is why
/// handwriting, photographs and perspective are absent and said to be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Perturb {
    /// Page rotations to cycle across documents, via the page's `/Rotate` entry.
    ///
    /// `/Rotate` is the cheapest honest perturbation there is: one dictionary key, the
    /// renderer does the work, and the label transform is a closed-form rotation. It is
    /// also the one that matters most, because `rotated` is **rank 0** in `select`'s
    /// class back-off ladder — the first condition it drops — and no page in this corpus
    /// has ever had a non-zero rotation, so that level of the ladder has never had data.
    pub rotate_deg: Vec<u16>,
    /// Page skews in degrees, cycled across documents, applied to the text matrix.
    ///
    /// The other half of what a scan does to a page: `/Rotate` covers the right angles a
    /// page can be *filed* at, and this covers the degree or two it was *fed* at. It is
    /// the perturbation `page.deskew` exists for, and it is the one whose labels are
    /// loose rather than exact — see `skew_label_iou`.
    pub skew_deg: Vec<f32>,
}

impl Perturb {
    /// Nothing, which is what the corpus did before and still does by default.
    pub fn none() -> Self {
        Self::default()
    }

    /// The four right angles, cycled.
    pub fn rotations() -> Self {
        Self { rotate_deg: vec![0, 90, 180, 270], skew_deg: Vec::new() }
    }

    /// The rotation for document `n`, or 0 when none were asked for.
    fn rotation_for(&self, n: usize) -> u16 {
        if self.rotate_deg.is_empty() {
            0
        } else {
            self.rotate_deg[n % self.rotate_deg.len()]
        }
    }

    /// The skew for document `n`, or 0 when none were asked for.
    ///
    /// Indexed independently of the rotation, so a corpus asked for two rotations and
    /// three skews gets all six combinations across six documents rather than the two
    /// cycles locking together.
    fn skew_for(&self, n: usize) -> f32 {
        if self.skew_deg.is_empty() {
            0.0
        } else {
            self.skew_deg[(n / self.rotate_deg.len().max(1)) % self.skew_deg.len()]
        }
    }
}

/// The four corners of a box, rotated about the page centre by `skew_deg`.
///
/// Counter-clockwise for a positive angle, in PDF user space, which is what the text
/// matrix below does to the glyphs.
fn skew_corners(bbox: [f32; 4], skew_deg: f32) -> [(f32, f32); 4] {
    let [x0, y0, x1, y1] = bbox;
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    if skew_deg == 0.0 {
        return corners;
    }
    let (s, c) = skew_deg.to_radians().sin_cos();
    let (cx, cy) = (PAGE_W / 2.0, PAGE_H / 2.0);
    corners.map(|(x, y)| {
        let (dx, dy) = (x - cx, y - cy);
        (cx + dx * c - dy * s, cy + dx * s + dy * c)
    })
}

/// The axis-aligned hull of a skewed box, in PDF user space.
fn skew_hull(bbox: [f32; 4], skew_deg: f32) -> [f32; 4] {
    let cs = skew_corners(bbox, skew_deg);
    let xs: Vec<f32> = cs.iter().map(|c| c.0).collect();
    let ys: Vec<f32> = cs.iter().map(|c| c.1).collect();
    let min = |v: &[f32]| v.iter().copied().fold(f32::INFINITY, f32::min);
    let max = |v: &[f32]| v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    [min(&xs), min(&ys), max(&xs), max(&ys)]
}

/// How much a skewed box's label *over*-covers the ink it describes: the ratio of the
/// true rotated rectangle's area to the area of the axis-aligned hull that labels it.
///
/// This is the number that keeps the skew perturbation honest. The ground truth for a
/// skewed page is the hull of a rotated box, because the label format is axis-aligned —
/// so a model returning the *correct* tilted region scores at most this IoU against it,
/// and `layout_f1` is pessimistic by a knowable amount rather than silently. It is worst
/// for a wide, thin box: at 2° a 450×60 paragraph bounds at about 0.79, while a single
/// 450×12 line bounds at about 0.43, which is below the 0.5 matcher threshold. That is
/// the reason the labels are emitted at paragraph granularity and the reason a skewed
/// corpus must never be compared against an unskewed one on layout.
pub fn skew_label_iou(bbox: [f32; 4], skew_deg: f32) -> f32 {
    let [x0, y0, x1, y1] = bbox;
    let (w, h) = (x1 - x0, y1 - y0);
    if w <= 0.0 || h <= 0.0 {
        return 1.0;
    }
    let hull = skew_hull(bbox, skew_deg);
    let hull_area = (hull[2] - hull[0]) * (hull[3] - hull[1]);
    if hull_area <= 0.0 {
        return 1.0;
    }
    // The rotated rectangle keeps its area; the hull contains it, so this is the IoU.
    (w * h / hull_area).clamp(0.0, 1.0)
}

/// Where a PDF-space box lands in the rendered image, and how big that image is.
///
/// `/Rotate` rotates the page **clockwise** for display, so the raster's dimensions swap
/// at 90 and 270 and the label has to follow. Derived rather than guessed, by tracking
/// one corner: unrotated, PDF `(x, y)` displays at `(x, H - y)` with the origin at the
/// top left; rotating that image 90° clockwise sends a pixel at `(u, v)` to `(H - v, u)`,
/// which composes to `(y, x)`. The other two angles follow the same way.
///
/// Returned normalised to `[0, 1]` against the rendered page, which is the convention
/// `score::GtPage` reads.
fn place(bbox: [f32; 4], rotate_deg: u16) -> ([f32; 4], (f32, f32)) {
    let [x0, y0, x1, y1] = bbox;
    // The four corners, so a rotation cannot silently un-order the box.
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    let (w, h) = (PAGE_W, PAGE_H);
    let (dims, mapped): ((f32, f32), Vec<(f32, f32)>) = match rotate_deg % 360 {
        90 => ((h, w), corners.iter().map(|&(x, y)| (y, x)).collect()),
        180 => ((w, h), corners.iter().map(|&(x, y)| (w - x, y)).collect()),
        270 => ((h, w), corners.iter().map(|&(x, y)| (h - y, w - x)).collect()),
        // Unrotated: the y flip from PDF's bottom-left origin to an image's top-left.
        _ => ((w, h), corners.iter().map(|&(x, y)| (x, h - y)).collect()),
    };
    let (pw, ph) = dims;
    let xs: Vec<f32> = mapped.iter().map(|c| c.0).collect();
    let ys: Vec<f32> = mapped.iter().map(|c| c.1).collect();
    let min = |v: &[f32]| v.iter().copied().fold(f32::INFINITY, f32::min);
    let max = |v: &[f32]| v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    ([min(&xs) / pw, min(&ys) / ph, max(&xs) / pw, max(&ys) / ph], dims)
}

/// How many documents of each shape, and how many pages each.
#[derive(Debug, Clone)]
pub struct Spec {
    pub docs_per_shape: usize,
    pub pages_per_doc: u32,
    /// Share of documents marked `holdout` rather than `train`, so a selection can be
    /// checked against pages it was not chosen on.
    pub holdout_share: f32,
    pub language: String,
    /// What is done to the pages before they are scored against. Default: nothing.
    pub perturb: Perturb,
}

impl Default for Spec {
    fn default() -> Self {
        Self { docs_per_shape: 3, pages_per_doc: 2, holdout_share: 0.34, language: "en".into(), perturb: Perturb::none() }
    }
}

/// What was written, for the caller to print.
#[derive(Debug, Clone, Serialize)]
pub struct Written {
    pub manifest: PathBuf,
    pub documents: usize,
    pub pages: usize,
    pub by_shape: BTreeMap<String, usize>,
    /// The worst IoU any labelled block can score against its own label, over the whole
    /// corpus, because of skew. `None` when nothing was skewed.
    ///
    /// Reported rather than left implicit: it is the ceiling on `layout_f1` for a model
    /// that is exactly right, so a skewed corpus's layout numbers are not comparable with
    /// an unskewed one's and this says by how much.
    pub skew_label_iou: Option<f32>,
}

/// Writes the corpus and its manifest under `dir`.
///
/// Deterministic: the same spec produces byte-identical PDFs, so a scoring run is
/// repeatable and a diff in results is a change in the code rather than in the corpus.
pub fn write(dir: &Path, spec: &Spec) -> anyhow::Result<Written> {
    let docs_dir = dir.join("docs");
    let gt_dir = dir.join("ground-truth");
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::create_dir_all(&gt_dir)?;

    let mut manifest_lines = Vec::new();
    let mut by_shape: BTreeMap<String, usize> = BTreeMap::new();
    let mut pages = 0usize;
    let mut worst_label_iou: Option<f32> = None;
    let shapes = [PageShape::Prose, PageShape::TwoColumn, PageShape::Form, PageShape::Table];

    for shape in shapes {
        for n in 0..spec.docs_per_shape {
            let id = format!("{}-{:02}", shape.as_str(), n + 1);
            let page_blocks: Vec<Vec<Block>> = (1..=spec.pages_per_doc).map(|p| blocks_for(shape, &id, p)).collect();
            // Cycled across the documents of each shape, so a shape is present both
            // upright and rotated and a conditioner can be scored against its own
            // ablation rather than against a different page.
            let rotate_deg = spec.perturb.rotation_for(n);
            let skew_deg = spec.perturb.skew_for(n);

            let pdf = docs_dir.join(format!("{id}.pdf"));
            std::fs::write(&pdf, pdf_bytes(&page_blocks, rotate_deg, skew_deg))?;

            let gt = gt_dir.join(format!("{id}.json"));
            std::fs::write(&gt, serde_json::to_vec_pretty(&ground_truth(&page_blocks, shape, rotate_deg, skew_deg))?)?;

            // The ceiling a correct answer can reach on this document's labels.
            if skew_deg != 0.0 {
                for b in page_blocks.iter().flatten() {
                    let iou = skew_label_iou(b.bbox(), skew_deg);
                    worst_label_iou = Some(worst_label_iou.map_or(iou, |w: f32| w.min(iou)));
                }
            }

            // `holdout` by position rather than at random, so the split is part of the
            // spec rather than of a seed — but *interleaved* rather than taken from the
            // tail, so split membership cannot correlate with the document index the
            // moment generation gains anything that varies along it.
            //
            // The tail form also rounded the wrong way: `(n + 1) > docs * (1 - share)`
            // held for n = 1 and n = 2 at the defaults, so a requested 0.34 holdout
            // share realised two documents of three. This picks exactly `holdout`
            // documents per shape and spreads them evenly.
            let holdout_docs = (spec.docs_per_shape as f32 * spec.holdout_share).round() as usize;
            let holdout = holdout_docs > 0 && (n * holdout_docs) / spec.docs_per_shape != ((n + 1) * holdout_docs) / spec.docs_per_shape;
            manifest_lines.push(json!({
                "uri": pdf.to_string_lossy(),
                "language": spec.language,
                // The label the classifier is scored against — and which a normal run
                // passes to routing as a caller's hint.
                "doc_type_hint": shape.doc_type(),
                "ground_truth_uri": gt.to_string_lossy(),
                "split": if holdout { "holdout" } else { "train" },
                // Recorded so a report can group by it. The classifier has to *detect*
                // the rotation from the image; this is the answer, not an input.
                "rotate_deg": rotate_deg,
                // As with the rotation: the answer, not an input. `page.deskew` has to
                // measure the angle off the ink.
                "skew_deg": skew_deg,
            }));
            *by_shape.entry(shape.as_str().to_string()).or_default() += 1;
            pages += spec.pages_per_doc as usize;
        }
    }

    let manifest = dir.join("manifest.jsonl");
    let body: String = manifest_lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&manifest, body)?;
    Ok(Written { manifest, documents: manifest_lines.len(), pages, by_shape, skew_label_iou: worst_label_iou })
}

/// The blocks of one page of one shape, each carrying the lines it was drawn from.
fn blocks_for(shape: PageShape, id: &str, page_no: u32) -> Vec<Block> {
    let l = |text: String, size: f32, x: f32, y: f32| Line { text, size, x, y };
    let one = |kind: &'static str, line: Line| Block { kind, lines: vec![line], grid: None };
    let top = PAGE_H - 72.0;
    let mut out = Vec::new();

    match shape {
        PageShape::Prose => {
            out.push(one("title", l(format!("Report {id}, page {page_no}"), 18.0, 72.0, top)));
            for section in 0..2u32 {
                let y = top - 48.0 - section as f32 * 220.0;
                out.push(one("heading", l(format!("{}.{} Findings", page_no, section + 1), 13.0, 72.0, y)));
                // One paragraph, four typeset lines.
                let lines: Vec<Line> = (0..4u32)
                    .map(|k| {
                        l(
                            format!("The measured value for sample {}{} was within the stated tolerance.", section + 1, k + 1),
                            10.0,
                            72.0,
                            y - 20.0 - k as f32 * 14.0,
                        )
                    })
                    .collect();
                out.push(Block { kind: "paragraph", lines, grid: None });
            }
        }
        PageShape::TwoColumn => {
            out.push(one("title", l(format!("A Study of {id}"), 18.0, 72.0, top)));
            // Two columns with a real gutter: 72..280 and 315..523. Each column is one
            // paragraph block, so a parser that merges the columns into one reading is
            // scored as having merged them rather than as having found ten lines.
            for (col, x) in [(0u32, 72.0f32), (1, 315.0)] {
                let lines: Vec<Line> = (0..10u32)
                    .map(|k| {
                        l(
                            format!("Column {} line {} of page {page_no} discusses the method.", col + 1, k + 1),
                            9.0,
                            x,
                            top - 48.0 - k as f32 * 13.0,
                        )
                    })
                    .collect();
                out.push(Block { kind: "paragraph", lines, grid: None });
            }
        }
        PageShape::Form => {
            out.push(one("title", l(format!("Application {id}"), 16.0, 72.0, top)));
            let fields = [
                ("Reference:", format!("REF-{}-{page_no}", id.to_uppercase())),
                ("Date:", "2026-03-01".to_string()),
                ("Applicant:", "A N Other".to_string()),
                ("Amount:", "1499.95".to_string()),
                ("Status:", "Pending".to_string()),
            ];
            // Label and value are separate runs at separate x positions, so they are
            // separate blocks: that is what a parser sees.
            for (i, (label, value)) in fields.iter().enumerate() {
                let y = top - 56.0 - i as f32 * 28.0;
                out.push(one("paragraph", l((*label).to_string(), 10.0, 72.0, y)));
                out.push(one("paragraph", l(value.clone(), 10.0, 220.0, y)));
            }
        }
        PageShape::Table => {
            out.push(one("heading", l(format!("Results for {id}"), 14.0, 72.0, top)));
            let header = ["Method", "Acc (%)", "F1"];
            let rows = [("Baseline", "88.4", "0.86"), ("Ours", "92.1", "0.90"), ("Ours+", "94.3", "0.93")];
            let y0 = top - 40.0;
            // One table block spanning the grid, not one block per cell. Per-cell labels
            // inverted the metric: `per_block_mean` matched a correct whole-table
            // prediction against every cell and scored each on the text of the whole
            // table, so a cell dump beat a correct reading.
            let mut lines = Vec::new();
            let mut grid = vec![header.iter().map(|h| (*h).to_string()).collect::<Vec<String>>()];
            for (c, h) in header.iter().enumerate() {
                lines.push(l((*h).to_string(), 10.0, 72.0 + c as f32 * 120.0, y0));
            }
            for (r, (m, acc, f1)) in rows.iter().enumerate() {
                let y = y0 - 18.0 - r as f32 * 16.0;
                for (c, cell) in [*m, *acc, *f1].iter().enumerate() {
                    lines.push(l((*cell).to_string(), 10.0, 72.0 + c as f32 * 120.0, y));
                }
                grid.push(vec![(*m).to_string(), (*acc).to_string(), (*f1).to_string()]);
            }
            out.push(Block { kind: "table", lines, grid: Some(grid) });
            out.push(one("caption", l(format!("Table {page_no}: accuracy on the held-out split."), 9.0, 72.0, y0 - 90.0)));
        }
    }
    out
}

/// The ground truth for a document: one entry per page, in the shape `score::GtPage`
/// reads, with boxes normalised to `[0,1]` and the y axis flipped to top-down.
///
/// Blocks are the scored unit; the lines each one was typeset into are nested under
/// `lines` for a reader that wants them. `score::GtBlock` ignores the extra field.
fn ground_truth(pages: &[Vec<Block>], shape: PageShape, rotate_deg: u16, skew_deg: f32) -> serde_json::Value {
    let doc_type = shape.doc_type();
    // Where the box lands in the **rendered** image, which `/Rotate` turns clockwise and
    // whose dimensions therefore swap at 90 and 270. The skew is applied first, in user
    // space, because that is the order the renderer sees: the content stream tilts the
    // glyphs and `/Rotate` then turns the whole page. Composing the other way round
    // would put every label on a rotated *and* skewed page in the wrong place.
    //
    // Hull-then-rotate equals rotate-then-hull here because `place`'s rotations are
    // multiples of 90° and so map axes onto axes.
    let norm = |b: [f32; 4]| {
        let (n, _) = place(skew_hull(b, skew_deg), rotate_deg);
        json!(n)
    };
    let (page_w, page_h) = place([0.0, 0.0, 0.0, 0.0], rotate_deg).1;
    let pages: Vec<serde_json::Value> = pages
        .iter()
        .map(|blocks| {
            let out: Vec<serde_json::Value> = blocks
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let lines: Vec<serde_json::Value> = b.lines.iter().map(|l| json!({ "text": l.text, "bbox": norm(l.bbox()) })).collect();
                    let mut v = json!({
                        "kind": b.kind,
                        "bbox": norm(b.bbox()),
                        "order": i,
                        "text": b.text(),
                        "lines": lines,
                    });
                    if let Some(html) = b.html() {
                        v["html"] = json!(html);
                    }
                    v
                })
                .collect();
            json!({ "blocks": out, "width_px": page_w, "height_px": page_h, "doc_type": doc_type })
        })
        .collect();
    json!({ "pages": pages })
}

// ───────────── the PDF itself ─────────────

/// A minimal multi-page PDF with one Helvetica font and the given lines.
///
/// Objects: catalogue, page tree, one page and one content stream per page, one font.
/// Written with a real cross-reference table, because pdfium reads it — a PDF that
/// "mostly parses" would make every failure here ambiguous.
fn pdf_bytes(pages: &[Vec<Block>], rotate_deg: u16, skew_deg: f32) -> Vec<u8> {
    let n = pages.len();
    // 1 catalogue, 2 pages tree, 3 font, then per page: page object and content stream.
    let page_obj = |i: usize| 4 + i * 2;
    let content_obj = |i: usize| 5 + i * 2;

    let mut objects: Vec<String> = Vec::new();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_obj(i))).collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" ")));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());

    for (i, blocks) in pages.iter().enumerate() {
        let mut content = String::new();
        // The skew goes into the text matrix rather than into the coordinates: a rotated
        // `Tm` turns the glyphs as well as moving them, which is what a page fed in
        // crooked looks like. Moving the origins alone would produce a staircase of
        // upright lines, and `page.deskew` measures the angle of the *ink*.
        let (sin, cos) = skew_deg.to_radians().sin_cos();
        let (cx, cy) = (PAGE_W / 2.0, PAGE_H / 2.0);
        for l in blocks.iter().flat_map(|b| b.lines.iter()) {
            // The baseline origin, turned about the page centre with everything else.
            let (dx, dy) = (l.x - cx, l.y - cy);
            let (x, y) = (cx + dx * cos - dy * sin, cy + dx * sin + dy * cos);
            // `Tj` with an escaped literal string: the text is ours, but escaping it is
            // still the correct thing to do rather than an assumption about it.
            //
            // The identity matrix is written in its short form so that an unskewed corpus
            // is byte-identical to the one every existing number was measured on — a
            // `1.000000 0.000000` spelling would change every document's content hash and
            // so every `document_hk`, for no reason.
            let matrix = if skew_deg == 0.0 { "1 0 0 1".to_string() } else { format!("{cos:.6} {sin:.6} {:.6} {cos:.6}", -sin) };
            content.push_str(&format!("BT /F1 {:.1} Tf {matrix} {x:.2} {y:.2} Tm ({}) Tj ET\n", l.size, escape(&l.text)));
        }
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] /Rotate {rotate_deg} /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            content_obj(i)
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}endstream", content.len()));
    }

    let mut out = String::from("%PDF-1.7\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
    }
    let xref_at = out.len();
    out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for off in &offsets {
        out.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
        objects.len() + 1
    ));
    out.into_bytes()
}

/// PDF literal-string escaping.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            // Outside WinAnsi there is no glyph in this font anyway; a '?' is a visible
            // wrong answer rather than a silently broken content stream.
            c if (c as u32) < 32 || (c as u32) > 126 => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skewed_label_is_the_hull_and_its_cost_is_known() {
        // A wide, thin box is the worst case, and the reason the labels are emitted at
        // paragraph granularity rather than per line.
        let line = [100.0, 400.0, 550.0, 412.0];
        let para = [100.0, 400.0, 550.0, 460.0];
        assert_eq!(skew_label_iou(para, 0.0), 1.0, "no skew costs nothing");
        let para_2 = skew_label_iou(para, 2.0);
        let line_2 = skew_label_iou(line, 2.0);
        assert!((0.75..0.85).contains(&para_2), "a paragraph at 2° bounds near 0.79, got {para_2}");
        assert!(line_2 < 0.5, "a single line at 2° falls below the matcher threshold, got {line_2}");
        // Symmetric in the sign, and monotonic in the angle: more skew is never cheaper.
        assert!((skew_label_iou(para, -2.0) - para_2).abs() < 1e-6);
        assert!(skew_label_iou(para, 4.0) < para_2);
    }

    #[test]
    fn skewing_turns_the_page_about_its_centre() {
        // The centre is the only fixed point, so a box there barely grows while one at a
        // corner moves a long way. Rotating about the origin instead would slide the
        // whole page off it, which is the mistake this pins.
        let centre = [PAGE_W / 2.0 - 20.0, PAGE_H / 2.0 - 20.0, PAGE_W / 2.0 + 20.0, PAGE_H / 2.0 + 20.0];
        let hull = skew_hull(centre, 2.0);
        assert!((hull[0] + hull[2]) / 2.0 - PAGE_W / 2.0 < 1e-3, "the centre stays put");
        let corner = [10.0, 10.0, 50.0, 50.0];
        let moved = skew_hull(corner, 2.0);
        assert!((moved[0] - corner[0]).abs() > 5.0, "a corner box moves: {moved:?}");
    }

    #[test]
    fn the_skewed_content_stream_carries_a_rotated_text_matrix() {
        let blocks = blocks_for(PageShape::Prose, "prose-01", 1);
        let upright = String::from_utf8_lossy(&pdf_bytes(std::slice::from_ref(&blocks), 0, 0.0)).to_string();
        let skewed = String::from_utf8_lossy(&pdf_bytes(&[blocks], 0, 2.0)).to_string();
        assert!(upright.contains("1 0 0 1 "), "an unskewed page keeps the identity matrix");
        assert!(!skewed.contains("1 0 0 1 "), "a skewed page must not");
        // The glyphs tilt, not only their origins: that is what `page.deskew` measures.
        assert!(skewed.contains("0.999391 0.034899 -0.034899 0.999391"), "no rotated Tm in {}", &skewed[..200.min(skewed.len())]);
    }

    #[test]
    fn rotation_and_skew_cycle_independently() {
        let p = Perturb { rotate_deg: vec![0, 90], skew_deg: vec![-2.0, 2.0] };
        // Four documents must give all four combinations, not two.
        let combos: Vec<(u16, f32)> = (0..4).map(|n| (p.rotation_for(n), p.skew_for(n))).collect();
        assert_eq!(combos, vec![(0, -2.0), (90, -2.0), (0, 2.0), (90, 2.0)], "{combos:?}");
    }

    #[test]
    fn a_generated_page_is_a_pdf_with_the_text_in_it() {
        let blocks = blocks_for(PageShape::Prose, "prose-01", 1);
        let bytes = pdf_bytes(&[blocks], 0, 0.0);
        assert!(bytes.starts_with(b"%PDF-1.7"), "not a PDF");
        assert!(bytes.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Type /Catalog") && text.contains("/Type /Page ") && text.contains("startxref"));
        assert!(text.contains("Findings"), "the page's own text must be in the content stream");
        // The format detector has to recognise it, or ingest refuses it before rendering.
        assert_eq!(ds_ingest::detect_format(&bytes), Some(ds_ingest::Format::Pdf));
    }

    #[test]
    fn the_same_spec_produces_the_same_bytes() {
        let a = pdf_bytes(&[blocks_for(PageShape::Table, "table-01", 1)], 0, 0.0);
        let b = pdf_bytes(&[blocks_for(PageShape::Table, "table-01", 1)], 0, 0.0);
        assert_eq!(a, b, "a corpus that changes between runs makes a score meaningless");
        assert_ne!(a, pdf_bytes(&[blocks_for(PageShape::Table, "table-02", 1)], 0, 0.0));
    }

    #[test]
    fn ground_truth_boxes_are_normalised_top_down_and_inside_the_page() {
        let blocks = blocks_for(PageShape::TwoColumn, "two_column-01", 1);
        let gt = ground_truth(&[blocks], PageShape::TwoColumn, 0, 0.0);
        let page = &gt["pages"][0];
        assert_eq!(page["doc_type"], json!("scientific_paper"));
        let blocks = page["blocks"].as_array().expect("blocks");
        // A title and one paragraph block per column: the unit a parser returns. The
        // ten typeset lines of each column are nested inside their block.
        assert_eq!(blocks.len(), 3, "title plus one paragraph per column");
        assert_eq!(blocks[1]["lines"].as_array().expect("lines").len(), 10);
        let mut saw_right_column = false;
        for b in blocks {
            let bb: Vec<f32> = b["bbox"].as_array().expect("bbox").iter().map(|v| v.as_f64().expect("num") as f32).collect();
            assert!(bb.iter().all(|v| (0.0..=1.0).contains(v)), "{bb:?} is outside the page");
            assert!(bb[0] < bb[2] && bb[1] < bb[3], "{bb:?} is not ordered");
            saw_right_column |= bb[0] > 0.5;
        }
        assert!(saw_right_column, "the second column must be on the right half of the page");
        // The title is the first block and sits at the top, which is what a top-down
        // y axis means — the easiest thing to get backwards.
        let title = &blocks[0];
        assert_eq!(title["kind"], json!("title"));
        assert!(title["bbox"][1].as_f64().expect("num") < 0.15, "the title is near the top");
    }

    #[test]
    fn the_corpus_is_written_with_a_manifest_and_both_splits() {
        let dir = std::env::temp_dir().join(format!("ds-adapt-ds-{}", ds_core::RunId::new()));
        let spec = Spec { docs_per_shape: 3, pages_per_doc: 2, ..Default::default() };
        let w = write(&dir, &spec).expect("write");
        assert_eq!(w.documents, 12, "four shapes x three documents");
        assert_eq!(w.pages, 24);
        assert_eq!(w.by_shape.len(), 4);

        let manifest = std::fs::read_to_string(&w.manifest).expect("manifest");
        let entries: Vec<crate::run::ManifestEntry> =
            manifest.lines().map(|l| serde_json::from_str(l).expect("a manifest entry")).collect();
        assert_eq!(entries.len(), 12);
        assert!(entries.iter().any(|e| e.split == "holdout"), "a selection has to be checkable on pages it was not chosen on");
        assert!(entries.iter().any(|e| e.split == "train"));
        for e in &entries {
            assert!(std::path::Path::new(&e.uri).exists(), "{} is missing", e.uri);
            let gt = e.ground_truth_uri.as_ref().expect("every document is labelled");
            assert!(std::path::Path::new(gt).exists());
            // The labels load through the same reader `score` uses, page by page.
            let page = crate::score::load_ground_truth(gt, 1).expect("readable").expect("page 1");
            assert!(!crate::score::gt_blocks(&page).is_empty());
            assert!(e.doc_type_hint.is_some(), "the classifier's label");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_paragraph_is_one_block_and_not_one_block_per_line() {
        let gt = ground_truth(&[blocks_for(PageShape::Prose, "prose-01", 1)], PageShape::Prose, 0, 0.0);
        let blocks = gt["pages"][0]["blocks"].as_array().expect("blocks").clone();
        // Title, then two sections of (heading, paragraph).
        assert_eq!(blocks.len(), 5);
        let kinds: Vec<&str> = blocks.iter().map(|b| b["kind"].as_str().expect("kind")).collect();
        assert_eq!(kinds, ["title", "heading", "paragraph", "heading", "paragraph"]);
        let para = &blocks[2];
        assert_eq!(para["lines"].as_array().expect("lines").len(), 4, "four typeset lines, one block");
        // The block's box is the hull of its lines, so it is taller than any one of them.
        let hull = para["bbox"].as_array().expect("bbox");
        let first = para["lines"][0]["bbox"].as_array().expect("bbox");
        let height = |b: &[serde_json::Value]| b[3].as_f64().expect("num") - b[1].as_f64().expect("num");
        assert!(height(hull) > height(first) * 3.0, "the hull spans every line");
    }

    #[test]
    fn a_table_is_one_block_carrying_real_html() {
        let gt = ground_truth(&[blocks_for(PageShape::Table, "table-01", 1)], PageShape::Table, 0, 0.0);
        let blocks = gt["pages"][0]["blocks"].as_array().expect("blocks").clone();
        let tables: Vec<&serde_json::Value> = blocks.iter().filter(|b| b["kind"] == json!("table")).collect();
        assert_eq!(tables.len(), 1, "one block for the table, not one per cell");
        let html = tables[0]["html"].as_str().expect("a table block carries html");
        assert!(html.starts_with("<table>") && html.contains("<td>Method</td>") && html.contains("<td>92.1</td>"));
        // Four rows of three cells, through the parser the metric uses.
        let parsed = crate::metrics::parse_table(html).expect("the metric's own parser reads it");
        assert_eq!(parsed.size(), 1 + 4 + 12, "table, four rows, twelve cells");
        // Without html the table metric cannot run at all.
        assert!(blocks.iter().all(|b| b["kind"] != json!("table") || b["html"].is_string()));
    }

    /// The rotation label transform, corner by corner.
    ///
    /// Worth testing exhaustively rather than by eye: a wrong sign here would label every
    /// rotated page in the wrong place, every DAG would score near zero on them, and the
    /// conclusion would read as "rotated pages cannot be parsed" rather than "the labels
    /// are upside down".
    #[test]
    fn a_rotated_page_labels_its_boxes_where_the_renderer_puts_them() {
        // The bottom-left corner of the page, a small square, in PDF space.
        let corner = [0.0, 0.0, 10.0, 10.0];

        // Unrotated: bottom-left of the page is bottom-left of the image (y flipped).
        let (b, dims) = place(corner, 0);
        assert_eq!(dims, (PAGE_W, PAGE_H));
        assert!(b[1] > 0.95 && b[3] > 0.99, "unrotated, it sits at the bottom: {b:?}");
        assert!(b[0] < 0.05, "and on the left: {b:?}");

        // 90° clockwise: the page's bottom-left corner moves to the image's top-left,
        // and the image is now landscape.
        let (b, dims) = place(corner, 90);
        assert_eq!(dims, (PAGE_H, PAGE_W), "a quarter turn swaps the raster's dimensions");
        assert!(b[0] < 0.05 && b[1] < 0.05, "90° puts it at the top left: {b:?}");

        // 180°: bottom-left goes to top-right.
        let (b, dims) = place(corner, 180);
        assert_eq!(dims, (PAGE_W, PAGE_H));
        assert!(b[2] > 0.95 && b[1] < 0.05, "180° puts it at the top right: {b:?}");

        // 270°: bottom-left goes to bottom-right.
        let (b, dims) = place(corner, 270);
        assert_eq!(dims, (PAGE_H, PAGE_W));
        assert!(b[2] > 0.95 && b[3] > 0.95, "270° puts it at the bottom right: {b:?}");

        // Whatever the angle: inside the page, correctly ordered, and the same area.
        let block = [72.0, 100.0, 523.0, 140.0];
        let (unrotated, _) = place(block, 0);
        let area = |b: [f32; 4]| (b[2] - b[0]) * (b[3] - b[1]);
        for deg in [0u16, 90, 180, 270] {
            let (b, _) = place(block, deg);
            assert!(b.iter().all(|v| (0.0..=1.0).contains(v)), "{deg}°: {b:?} is outside the page");
            assert!(b[0] < b[2] && b[1] < b[3], "{deg}°: {b:?} is not ordered");
            // Normalised area is preserved because the page's own dimensions rotate with
            // it — a 90° turn that forgot to swap them would show up here.
            assert!((area(b) - area(unrotated)).abs() < 1e-6, "{deg}°: area moved from {:?} to {b:?}", unrotated);
        }

        // Four right angles return the box to where it started.
        let mut b = place(block, 0).0;
        for _ in 0..4 {
            b = place(block, 360).0;
        }
        assert_eq!(b, unrotated, "a full turn is no turn");
    }

    #[test]
    fn rotations_are_cycled_across_a_shape_s_documents_and_recorded() {
        let dir = std::env::temp_dir().join(format!("ds-adapt-rot-{}", ds_core::RunId::new()));
        let spec = Spec { docs_per_shape: 4, pages_per_doc: 1, holdout_share: 0.0, perturb: Perturb::rotations(), ..Default::default() };
        write(&dir, &spec).expect("write");
        let manifest = std::fs::read_to_string(dir.join("manifest.jsonl")).expect("manifest");

        let mut by_shape: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
        for line in manifest.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("entry");
            let uri = v["uri"].as_str().expect("uri").to_string();
            let shape = uri.rsplit('/').next().and_then(|f| f.split('-').next()).expect("shape").to_string();
            by_shape.entry(shape).or_default().push(v["rotate_deg"].as_u64().expect("recorded"));
        }
        for (shape, degs) in &by_shape {
            assert_eq!(degs, &vec![0, 90, 180, 270], "{shape} must carry every angle");
        }

        // And the labels for a rotated document describe a landscape page.
        let gt: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("ground-truth/prose-02.json")).expect("gt")).expect("json");
        let page = &gt["pages"][0];
        assert_eq!(page["width_px"], json!(PAGE_H), "a quarter-turned page is wider than it is tall");
        assert_eq!(page["height_px"], json!(PAGE_W));
        // Which the scorer's own reader has to agree with, since it divides by these.
        let loaded = crate::score::load_ground_truth(dir.join("ground-truth/prose-02.json").to_str().expect("path"), 1)
            .expect("readable")
            .expect("page 1");
        assert!(!crate::score::gt_blocks(&loaded).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unperturbed_corpus_is_exactly_what_it_was() {
        // The default must stay clean and upright: every existing number was measured on
        // it, and a perturbation switched on by default would silently move them all.
        assert_eq!(Spec::default().perturb, Perturb::none());
        let plain = pdf_bytes(&[blocks_for(PageShape::Prose, "prose-01", 1)], 0, 0.0);
        assert!(String::from_utf8_lossy(&plain).contains("/Rotate 0"));
    }

    #[test]
    fn the_holdout_share_is_the_share_that_was_asked_for() {
        // The tail form held for n = 1 and n = 2 at three documents, so a 0.34 share
        // realised two thirds of the corpus as holdout.
        for (docs, share, want_holdout) in [(3usize, 0.34f32, 1usize), (3, 0.67, 2), (10, 0.3, 3), (4, 0.0, 0), (5, 1.0, 5)] {
            let dir = std::env::temp_dir().join(format!("ds-adapt-split-{}", ds_core::RunId::new()));
            let spec = Spec { docs_per_shape: docs, pages_per_doc: 1, holdout_share: share, ..Default::default() };
            write(&dir, &spec).expect("write");
            let manifest = std::fs::read_to_string(dir.join("manifest.jsonl")).expect("manifest");
            let entries: Vec<crate::run::ManifestEntry> =
                manifest.lines().map(|l| serde_json::from_str(l).expect("entry")).collect();
            for shape in ["prose", "two_column", "form", "table"] {
                let n = entries
                    .iter()
                    .filter(|e| e.uri.contains(&format!("/{shape}-")) && e.split == "holdout")
                    .count();
                assert_eq!(n, want_holdout, "{docs} documents at share {share}, shape {shape}");
            }
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
