//! A ground-truthed corpus, generated.
//!
//! `vs-adapt`'s status said it plainly: *"No dataset exists in-repo or is referenced; the
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

use vs_core::DocType;

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
    kind: &'static str,
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

/// How many documents of each shape, and how many pages each.
#[derive(Debug, Clone)]
pub struct Spec {
    pub docs_per_shape: usize,
    pub pages_per_doc: u32,
    /// Share of documents marked `holdout` rather than `train`, so a selection can be
    /// checked against pages it was not chosen on.
    pub holdout_share: f32,
    pub language: String,
}

impl Default for Spec {
    fn default() -> Self {
        Self { docs_per_shape: 3, pages_per_doc: 2, holdout_share: 0.34, language: "en".into() }
    }
}

/// What was written, for the caller to print.
#[derive(Debug, Clone, Serialize)]
pub struct Written {
    pub manifest: PathBuf,
    pub documents: usize,
    pub pages: usize,
    pub by_shape: BTreeMap<String, usize>,
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
    let shapes = [PageShape::Prose, PageShape::TwoColumn, PageShape::Form, PageShape::Table];

    for shape in shapes {
        for n in 0..spec.docs_per_shape {
            let id = format!("{}-{:02}", shape.as_str(), n + 1);
            let page_lines: Vec<Vec<Line>> = (1..=spec.pages_per_doc).map(|p| lines_for(shape, &id, p)).collect();

            let pdf = docs_dir.join(format!("{id}.pdf"));
            std::fs::write(&pdf, pdf_bytes(&page_lines))?;

            let gt = gt_dir.join(format!("{id}.json"));
            std::fs::write(&gt, serde_json::to_vec_pretty(&ground_truth(&page_lines, shape))?)?;

            // `holdout` by position rather than at random, so the split is part of the
            // spec rather than of a seed.
            let holdout = (n as f32 + 1.0) > spec.docs_per_shape as f32 * (1.0 - spec.holdout_share);
            manifest_lines.push(json!({
                "uri": pdf.to_string_lossy(),
                "language": spec.language,
                // The label the classifier is scored against — and which a normal run
                // passes to routing as a caller's hint.
                "doc_type_hint": shape.doc_type(),
                "ground_truth_uri": gt.to_string_lossy(),
                "split": if holdout { "holdout" } else { "train" },
            }));
            *by_shape.entry(shape.as_str().to_string()).or_default() += 1;
            pages += spec.pages_per_doc as usize;
        }
    }

    let manifest = dir.join("manifest.jsonl");
    let body: String = manifest_lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&manifest, body)?;
    Ok(Written { manifest, documents: manifest_lines.len(), pages, by_shape })
}

/// The lines of one page of one shape.
fn lines_for(shape: PageShape, id: &str, page_no: u32) -> Vec<Line> {
    let mut out = Vec::new();
    let line = |text: String, kind: &'static str, size: f32, x: f32, y: f32| Line { text, kind, size, x, y };
    let top = PAGE_H - 72.0;

    match shape {
        PageShape::Prose => {
            out.push(line(format!("Report {id}, page {page_no}"), "title", 18.0, 72.0, top));
            for section in 0..2u32 {
                let y = top - 48.0 - section as f32 * 220.0;
                out.push(line(format!("{}.{} Findings", page_no, section + 1), "heading", 13.0, 72.0, y));
                for l in 0..4u32 {
                    out.push(line(
                        format!("The measured value for sample {}{} was within the stated tolerance.", section + 1, l + 1),
                        "paragraph",
                        10.0,
                        72.0,
                        y - 20.0 - l as f32 * 14.0,
                    ));
                }
            }
        }
        PageShape::TwoColumn => {
            out.push(line(format!("A Study of {id}"), "title", 18.0, 72.0, top));
            // Two columns with a real gutter: 72..280 and 315..523.
            for (col, x) in [(0u32, 72.0f32), (1, 315.0)] {
                for l in 0..10u32 {
                    out.push(line(
                        format!("Column {} line {} of page {page_no} discusses the method.", col + 1, l + 1),
                        "paragraph",
                        9.0,
                        x,
                        top - 48.0 - l as f32 * 13.0,
                    ));
                }
            }
        }
        PageShape::Form => {
            out.push(line(format!("Application {id}"), "title", 16.0, 72.0, top));
            let fields = [
                ("Reference:", format!("REF-{}-{page_no}", id.to_uppercase())),
                ("Date:", "2026-03-01".to_string()),
                ("Applicant:", "A N Other".to_string()),
                ("Amount:", "1499.95".to_string()),
                ("Status:", "Pending".to_string()),
            ];
            for (i, (label, value)) in fields.iter().enumerate() {
                let y = top - 56.0 - i as f32 * 28.0;
                out.push(line((*label).to_string(), "paragraph", 10.0, 72.0, y));
                out.push(line(value.clone(), "paragraph", 10.0, 220.0, y));
            }
        }
        PageShape::Table => {
            out.push(line(format!("Results for {id}"), "heading", 14.0, 72.0, top));
            let header = ["Method", "Acc (%)", "F1"];
            let rows = [("Baseline", "88.4", "0.86"), ("Ours", "92.1", "0.90"), ("Ours+", "94.3", "0.93")];
            let y0 = top - 40.0;
            for (c, h) in header.iter().enumerate() {
                out.push(line((*h).to_string(), "table", 10.0, 72.0 + c as f32 * 120.0, y0));
            }
            for (r, (m, acc, f1)) in rows.iter().enumerate() {
                let y = y0 - 18.0 - r as f32 * 16.0;
                for (c, cell) in [*m, *acc, *f1].iter().enumerate() {
                    out.push(line((*cell).to_string(), "table", 10.0, 72.0 + c as f32 * 120.0, y));
                }
            }
            out.push(line(format!("Table {page_no}: accuracy on the held-out split."), "caption", 9.0, 72.0, y0 - 90.0));
        }
    }
    out
}

/// The ground truth for a document: one entry per page, in the shape `score::GtPage`
/// reads, with boxes normalised to `[0,1]` and the y axis flipped to top-down.
fn ground_truth(pages: &[Vec<Line>], shape: PageShape) -> serde_json::Value {
    let doc_type = shape.doc_type();
    let pages: Vec<serde_json::Value> = pages
        .iter()
        .map(|lines| {
            let blocks: Vec<serde_json::Value> = lines
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let [x0, y0, x1, y1] = l.bbox();
                    json!({
                        "kind": l.kind,
                        // PDF user space is bottom-left; a page image is top-left.
                        "bbox": [x0 / PAGE_W, (PAGE_H - y1) / PAGE_H, x1 / PAGE_W, (PAGE_H - y0) / PAGE_H],
                        "order": i,
                        "text": l.text,
                    })
                })
                .collect();
            json!({ "blocks": blocks, "width_px": PAGE_W, "height_px": PAGE_H, "doc_type": doc_type })
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
fn pdf_bytes(pages: &[Vec<Line>]) -> Vec<u8> {
    let n = pages.len();
    // 1 catalogue, 2 pages tree, 3 font, then per page: page object and content stream.
    let page_obj = |i: usize| 4 + i * 2;
    let content_obj = |i: usize| 5 + i * 2;

    let mut objects: Vec<String> = Vec::new();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_obj(i))).collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" ")));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());

    for (i, lines) in pages.iter().enumerate() {
        let mut content = String::new();
        for l in lines {
            // `Tj` with an escaped literal string: the text is ours, but escaping it is
            // still the correct thing to do rather than an assumption about it.
            content.push_str(&format!(
                "BT /F1 {:.1} Tf 1 0 0 1 {:.2} {:.2} Tm ({}) Tj ET\n",
                l.size,
                l.x,
                l.y,
                escape(&l.text)
            ));
        }
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
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
    fn a_generated_page_is_a_pdf_with_the_text_in_it() {
        let lines = lines_for(PageShape::Prose, "prose-01", 1);
        let bytes = pdf_bytes(&[lines]);
        assert!(bytes.starts_with(b"%PDF-1.7"), "not a PDF");
        assert!(bytes.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Type /Catalog") && text.contains("/Type /Page ") && text.contains("startxref"));
        assert!(text.contains("Findings"), "the page's own text must be in the content stream");
        // The format detector has to recognise it, or ingest refuses it before rendering.
        assert_eq!(vs_ingest::detect_format(&bytes), Some(vs_ingest::Format::Pdf));
    }

    #[test]
    fn the_same_spec_produces_the_same_bytes() {
        let a = pdf_bytes(&[lines_for(PageShape::Table, "table-01", 1)]);
        let b = pdf_bytes(&[lines_for(PageShape::Table, "table-01", 1)]);
        assert_eq!(a, b, "a corpus that changes between runs makes a score meaningless");
        assert_ne!(a, pdf_bytes(&[lines_for(PageShape::Table, "table-02", 1)]));
    }

    #[test]
    fn ground_truth_boxes_are_normalised_top_down_and_inside_the_page() {
        let lines = lines_for(PageShape::TwoColumn, "two_column-01", 1);
        let gt = ground_truth(&[lines], PageShape::TwoColumn);
        let page = &gt["pages"][0];
        assert_eq!(page["doc_type"], json!("scientific_paper"));
        let blocks = page["blocks"].as_array().expect("blocks");
        assert!(blocks.len() > 20, "a two-column page has two columns of lines");
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
        let dir = std::env::temp_dir().join(format!("vs-adapt-ds-{}", vs_core::RunId::new()));
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
}
