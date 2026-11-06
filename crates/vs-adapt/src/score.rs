//! `score`: runs + ground truth (or judge) → `PageEval` per page × DAG (SPEC.md "Scoring").

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vs_core::{BBox, BlockContent, BlockKind};
use vs_dag::BlockDraft;
use vs_models::{ChatRequest, OpenAiClient};

use crate::metrics::{self, EvalBlock};
use crate::records::{AdaptPageResult, RunRecord};
use crate::select::{Metrics, PageEval, Trust};

// ---------------------------------------------------------------------------
// Ground truth
// ---------------------------------------------------------------------------

/// Bounding box in the ground truth: `[x0, y0, x1, y1]` (normalised, or pixels when the
/// page gives `width_px`/`height_px`) or an object with the same fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GtBBox {
    Array([f32; 4]),
    Object { x0: f32, y0: f32, x1: f32, y1: f32 },
    /// OmniDocBench polygon `[x0,y0, x1,y0, x1,y1, x0,y1]`.
    Poly(Vec<f32>),
}

impl GtBBox {
    fn corners(&self) -> Option<(f32, f32, f32, f32)> {
        match self {
            GtBBox::Array([a, b, c, d]) => Some((*a, *b, *c, *d)),
            GtBBox::Object { x0, y0, x1, y1 } => Some((*x0, *y0, *x1, *y1)),
            GtBBox::Poly(p) if p.len() >= 4 && p.len() % 2 == 0 => {
                let xs = p.iter().step_by(2);
                let ys = p.iter().skip(1).step_by(2);
                Some((
                    xs.clone().cloned().fold(f32::MAX, f32::min),
                    ys.clone().cloned().fold(f32::MAX, f32::min),
                    xs.cloned().fold(f32::MIN, f32::max),
                    ys.cloned().fold(f32::MIN, f32::max),
                ))
            }
            _ => None,
        }
    }
}

/// One ground-truth block (OmniDocBench-like).
#[derive(Debug, Clone, Deserialize)]
pub struct GtBlock {
    #[serde(alias = "category", alias = "category_type", alias = "label")]
    pub kind: String,
    #[serde(alias = "poly")]
    pub bbox: GtBBox,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub latex: Option<String>,
    #[serde(default)]
    pub order: Option<u32>,
}

/// One ground-truth page.
#[derive(Debug, Clone, Deserialize)]
pub struct GtPage {
    #[serde(alias = "layout_dets")]
    pub blocks: Vec<GtBlock>,
    #[serde(default)]
    pub width_px: Option<f32>,
    #[serde(default)]
    pub height_px: Option<f32>,
}

/// Maps a ground-truth label to a `BlockKind`: our snake_case names, then the shared
/// label mapping of the parsers, then common benchmark categories.
pub fn gt_kind(label: &str) -> BlockKind {
    if let Ok(k) = serde_json::from_value::<BlockKind>(Value::String(label.to_string())) {
        return k;
    }
    let lower = label.to_ascii_lowercase();
    match lower.as_str() {
        "text_block" | "text_mask" | "text" => BlockKind::Paragraph,
        "equation_isolated" | "equation_inline" | "equation_ignore" => BlockKind::Formula,
        "code_txt" | "code_txt_caption" => BlockKind::Code,
        "page_footnote" | "footnote" => BlockKind::Footnote,
        "table_footnote" | "figure_footnote" => BlockKind::Caption,
        "abandon" => BlockKind::Other,
        _ => vs_models::parsers::block_kind_from_label(label),
    }
}

fn normalise_bbox(c: (f32, f32, f32, f32), page: &GtPage) -> Option<BBox> {
    let (mut x0, mut y0, mut x1, mut y1) = c;
    let pixel = [x0, y0, x1, y1].iter().any(|v| *v > 1.0);
    if pixel {
        let (w, h) = (page.width_px?, page.height_px?);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        x0 /= w;
        x1 /= w;
        y0 /= h;
        y1 /= h;
    }
    let (x0, x1) = (x0.min(x1), x0.max(x1));
    let (y0, y1) = (y0.min(y1), y0.max(y1));
    BBox::new(x0.clamp(0.0, 1.0), y0.clamp(0.0, 1.0), x1.clamp(0.0, 1.0), y1.clamp(0.0, 1.0)).ok()
}

/// Converts a ground-truth page to metric blocks. Blocks without a usable bbox are
/// dropped; a missing `order` falls back to file order.
pub fn gt_blocks(page: &GtPage) -> Vec<EvalBlock> {
    page.blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let bbox = normalise_bbox(b.bbox.corners()?, page)?;
            let kind = gt_kind(&b.kind);
            let text = b
                .text
                .clone()
                .or_else(|| match kind {
                    BlockKind::Formula => b.latex.clone(),
                    _ => None,
                })
                .unwrap_or_default();
            Some(EvalBlock { kind, bbox, order: b.order.unwrap_or(i as u32), text, html: b.html.clone(), latex: b.latex.clone() })
        })
        .collect()
}

/// Loads the ground truth for one page. `uri` is a local path to either a JSON file
/// (`{"pages": [...]}`, a bare array of pages, or a single page object) or a directory
/// holding `<page>.json` / `page-<page>.json`. `page_no` is 1-based.
pub fn load_ground_truth(uri: &str, page_no: u32) -> anyhow::Result<Option<GtPage>> {
    let path = Path::new(uri);
    if path.is_dir() {
        for name in [format!("{page_no}.json"), format!("page-{page_no}.json"), format!("page_{page_no}.json")] {
            let p = path.join(name);
            if p.exists() {
                return Ok(Some(serde_json::from_str(&std::fs::read_to_string(&p)?)?));
            }
        }
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading ground truth {uri}: {e}"))?;
    let v: Value = serde_json::from_str(&text)?;
    let page = match &v {
        Value::Array(pages) => pages.get(page_no as usize - 1).cloned(),
        Value::Object(o) if o.contains_key("pages") => o["pages"].as_array().and_then(|p| p.get(page_no as usize - 1).cloned()),
        Value::Object(_) if page_no == 1 => Some(v.clone()),
        _ => None,
    };
    Ok(match page {
        Some(p) => Some(serde_json::from_value(p)?),
        None => None,
    })
}

// ---------------------------------------------------------------------------
// Predictions
// ---------------------------------------------------------------------------

pub fn draft_blocks(drafts: &[BlockDraft]) -> Vec<EvalBlock> {
    drafts
        .iter()
        .map(|d| {
            let (html, latex) = match &d.content {
                BlockContent::Table { html } => (Some(html.clone()), None),
                BlockContent::Formula { latex } => (None, Some(latex.clone())),
                BlockContent::Chart { data_html, .. } => (data_html.clone(), None),
                _ => (None, None),
            };
            EvalBlock { kind: d.kind, bbox: d.bbox, order: d.reading_order, text: d.text.clone(), html, latex }
        })
        .collect()
}

/// Markdown rendering of a page result (judge input).
pub fn blocks_to_markdown(drafts: &[BlockDraft]) -> String {
    let mut ds: Vec<&BlockDraft> = drafts.iter().collect();
    ds.sort_by_key(|d| d.reading_order);
    let mut out = String::new();
    for d in ds {
        match (&d.kind, &d.content) {
            (BlockKind::Title, _) => out.push_str(&format!("# {}\n\n", d.text.trim())),
            (BlockKind::Heading, _) => out.push_str(&format!("{} {}\n\n", "#".repeat(d.level.unwrap_or(2).clamp(1, 6) as usize), d.text.trim())),
            (_, BlockContent::Table { html }) => out.push_str(&format!("{}\n\n", html.trim())),
            (_, BlockContent::Formula { latex }) => out.push_str(&format!("$$\n{}\n$$\n\n", latex.trim())),
            (_, BlockContent::Figure { description, .. }) => out.push_str(&format!("![figure]({})\n\n", description.as_deref().unwrap_or("").trim())),
            (_, BlockContent::Chart { description, data_html, .. }) => {
                out.push_str(&format!("![chart]({})\n\n", description.as_deref().unwrap_or("").trim()));
                if let Some(h) = data_html {
                    out.push_str(&format!("{}\n\n", h.trim()));
                }
            }
            _ => {
                if !d.text.trim().is_empty() {
                    out.push_str(&format!("{}\n\n", d.text.trim()));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Metrics → quality
// ---------------------------------------------------------------------------

/// Weights of the applicable metrics in `quality`, and of the judge against agreement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricWeights {
    pub text: f32,
    pub tables: f32,
    pub formulas: f32,
    pub reading_order: f32,
    pub layout: f32,
    /// Share of the judge rating in judge-trust quality (rest = agreement).
    pub judge: f32,
}

impl Default for MetricWeights {
    fn default() -> Self {
        Self { text: 1.0, tables: 1.0, formulas: 1.0, reading_order: 0.5, layout: 0.5, judge: 0.5 }
    }
}

/// All labelled metrics of one prediction against one ground-truth page.
pub fn labelled_metrics(pred: &[EvalBlock], gt: &[EvalBlock]) -> Metrics {
    let gt_text = metrics::reading_order_text(gt);
    let has_text = gt.iter().any(|b| metrics::is_text_kind(b.kind));
    Metrics {
        text: has_text.then(|| metrics::normalised_text_similarity(&metrics::reading_order_text(pred), &gt_text)),
        tables: metrics::per_block_mean(pred, gt, BlockKind::Table, |p, g| match (&p.html, &g.html) {
            (Some(ph), Some(gh)) => metrics::teds(ph, gh),
            // ground truth without HTML: fall back to cell text similarity
            (_, None) => metrics::normalised_text_similarity(&p.text, &g.text),
            (None, Some(_)) => 0.0,
        }),
        formulas: metrics::per_block_mean(pred, gt, BlockKind::Formula, |p, g| {
            let pl = p.latex.as_deref().unwrap_or(&p.text);
            let gl = g.latex.as_deref().unwrap_or(&g.text);
            metrics::latex_similarity(pl, gl)
        }),
        reading_order: (gt.len() >= 2).then(|| metrics::reading_order_score(pred, gt)),
        layout: (!gt.is_empty()).then(|| metrics::layout_f1(pred, gt)),
        judge: None,
        agreement: None,
    }
}

/// Weighted mean of the applicable metrics; 0 when none applies.
pub fn quality_from_metrics(m: &Metrics, w: &MetricWeights) -> f32 {
    let parts = [(m.text, w.text), (m.tables, w.tables), (m.formulas, w.formulas), (m.reading_order, w.reading_order), (m.layout, w.layout)];
    let (sum, wsum) = parts.iter().filter_map(|(v, w)| v.map(|v| (v * w, *w))).fold((0.0, 0.0), |(s, ws), (v, w)| (s + v, ws + w));
    if wsum <= 0.0 { 0.0 } else { (sum / wsum).clamp(0.0, 1.0) }
}

// ---------------------------------------------------------------------------
// Judge
// ---------------------------------------------------------------------------

pub struct Judge {
    pub client: Arc<OpenAiClient>,
    pub model: String,
}

pub const JUDGE_PROMPT: &str = "You are grading a document-parsing system. The image is a scanned or rendered document page; \
the Markdown below is what the system extracted from it (tables as HTML, formulas as LaTeX). \
Rate how faithfully the Markdown reproduces ALL content of the page: text (completeness and correctness), \
reading order, table structure and cell values, formulas, and whether figures are noted. \
Ignore styling. Answer with JSON {\"score\": s} where s is a number from 0 (nothing usable) to 1 (complete and correct).\n\n\
Extracted Markdown:\n```markdown\n{markdown}\n```";

impl Judge {
    pub async fn rate(&self, image_png: Arc<[u8]>, markdown: &str) -> anyhow::Result<f32> {
        let mut md = markdown.to_string();
        if md.chars().count() > 12_000 {
            md = md.chars().take(12_000).collect::<String>() + "\n…(truncated)";
        }
        let resp = self
            .client
            .chat(&self.model, &ChatRequest {
                prompt: JUDGE_PROMPT.replace("{markdown}", &md),
                images: vec![(image_png, "image/png")],
                max_tokens: 50,
                json_schema: Some(json!({"type": "object", "properties": {"score": {"type": "number", "minimum": 0, "maximum": 1}}, "required": ["score"]})),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("judge: {e}"))?;
        let v: Value = serde_json::from_str(&resp.text)?;
        v["score"].as_f64().map(|s| (s as f32).clamp(0.0, 1.0)).ok_or_else(|| anyhow::anyhow!("judge returned no score: {}", resp.text))
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Loaded prediction for one run record.
struct Loaded {
    rec: RunRecord,
    result: Option<AdaptPageResult>,
}

fn load_result(rec: &RunRecord, base: Option<&Path>) -> Option<AdaptPageResult> {
    let p = rec.result_path.as_ref()?;
    let path = resolve(p, base);
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| tracing::warn!(path = %path.display(), %e, "bad result file")).ok(),
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "missing result file");
            None
        }
    }
}

/// Paths in `runs.jsonl` are relative to the run's `--out` directory; `base` resolves them.
fn resolve(p: &str, base: Option<&Path>) -> PathBuf {
    let path = Path::new(p);
    match base {
        Some(b) if path.is_relative() && !path.exists() => b.join(path),
        _ => path.to_path_buf(),
    }
}

fn base_eval(rec: &RunRecord, trust: Trust) -> PageEval {
    let mut e = PageEval::new(format!("{}/{}", rec.doc, rec.page), rec.profile_json.clone(), rec.dag_id.clone(), 0.0, rec.latency_ms, rec.gpu_seconds, trust);
    e.dag_version = Some(rec.dag_version.clone());
    e.doc = Some(rec.doc.clone());
    e.page_no = Some(rec.page);
    e.class_key = Some(rec.class_key.clone());
    e.tokens = rec.tokens;
    e.failed = !rec.ok;
    e
}

/// Labelled evaluation of one record (quality from ground truth).
pub fn score_labelled(rec: &RunRecord, result: Option<&AdaptPageResult>, gt: &[EvalBlock], w: &MetricWeights) -> PageEval {
    let mut e = base_eval(rec, Trust::Labelled);
    if let (true, Some(r)) = (rec.ok, result) {
        e.metrics = labelled_metrics(&draft_blocks(&r.blocks), gt);
        e.quality = quality_from_metrics(&e.metrics, w);
    }
    e
}

/// Reference DAG for judge-trust pages: the DAG with the highest mean labelled quality
/// among `ok_dags` if any labelled evals exist, otherwise the medoid by text similarity.
pub fn pick_reference<'a>(ok_dags: &[(&'a str, &str)], labelled_mean: &HashMap<String, f32>) -> Option<&'a str> {
    if ok_dags.is_empty() {
        return None;
    }
    let mut by_label: Vec<(&str, f32)> = ok_dags.iter().filter_map(|(d, _)| labelled_mean.get(*d).map(|q| (*d, *q))).collect();
    if !by_label.is_empty() {
        by_label.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(b.0)));
        return Some(by_label[0].0);
    }
    let mut best: Option<(&str, f32)> = None;
    for (d, text) in ok_dags {
        let mean = ok_dags.iter().filter(|(o, _)| o != d).map(|(_, t)| metrics::text_similarity(text, t)).sum::<f32>() / (ok_dags.len().max(2) - 1) as f32;
        if best.map_or(true, |(bd, bm)| mean > bm || (mean == bm && *d < bd)) {
            best = Some((d, mean));
        }
    }
    best.map(|(d, _)| d)
}

pub struct ScoreOptions<'a> {
    pub weights: MetricWeights,
    pub judge: Option<&'a Judge>,
    /// Directory the run wrote to (resolves relative paths in `runs.jsonl`).
    pub base: Option<&'a Path>,
}

/// Scores every run record. Pages with ground truth → `labelled`; others → `judge`
/// (agreement with the reference DAG, optionally mixed with the judge rating).
pub async fn score(runs: Vec<RunRecord>, opts: &ScoreOptions<'_>) -> anyhow::Result<Vec<PageEval>> {
    let loaded: Vec<Loaded> = runs.into_iter().map(|rec| Loaded { result: load_result(&rec, opts.base), rec }).collect();

    // group by page
    let mut pages: BTreeMap<(String, u32), Vec<&Loaded>> = BTreeMap::new();
    for l in &loaded {
        pages.entry((l.rec.doc.clone(), l.rec.page)).or_default().push(l);
    }

    let mut evals: Vec<PageEval> = vec![];
    let mut labelled_sum: HashMap<String, (f32, usize)> = HashMap::new();
    let mut unlabelled: Vec<&Vec<&Loaded>> = vec![];
    let mut gt_cache: HashMap<(String, u32), Option<Vec<EvalBlock>>> = HashMap::new();

    for (key, recs) in &pages {
        let gt_uri = recs.iter().find_map(|l| l.rec.ground_truth_uri.clone());
        let gt = match gt_uri {
            Some(uri) => gt_cache
                .entry(key.clone())
                .or_insert_with(|| match load_ground_truth(&uri, key.1) {
                    Ok(Some(p)) => Some(gt_blocks(&p)),
                    Ok(None) => {
                        tracing::warn!(doc = %key.0, page = key.1, "no ground truth page in {uri}");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(doc = %key.0, page = key.1, %e, "ground truth unreadable");
                        None
                    }
                })
                .clone(),
            None => None,
        };
        match gt {
            Some(gt) => {
                for l in recs {
                    let e = score_labelled(&l.rec, l.result.as_ref(), &gt, &opts.weights);
                    let s = labelled_sum.entry(e.dag_id.clone()).or_default();
                    s.0 += e.quality;
                    s.1 += 1;
                    evals.push(e);
                }
            }
            None => unlabelled.push(recs),
        }
    }

    let labelled_mean: HashMap<String, f32> = labelled_sum.into_iter().map(|(d, (s, n))| (d, s / n as f32)).collect();
    for recs in unlabelled {
        let texts: Vec<(&str, String)> = recs
            .iter()
            .filter(|l| l.rec.ok)
            .filter_map(|l| l.result.as_ref().map(|r| (l.rec.dag_id.as_str(), metrics::reading_order_text(&draft_blocks(&r.blocks)))))
            .collect();
        let ok_dags: Vec<(&str, &str)> = texts.iter().map(|(d, t)| (*d, t.as_str())).collect();
        let reference = pick_reference(&ok_dags, &labelled_mean);
        let ref_text = reference.and_then(|r| texts.iter().find(|(d, _)| *d == r)).map(|(_, t)| t.as_str()).unwrap_or("");
        for l in recs {
            let mut e = base_eval(&l.rec, Trust::Judge);
            if let (true, Some(r)) = (l.rec.ok, l.result.as_ref()) {
                let text = metrics::reading_order_text(&draft_blocks(&r.blocks));
                let agreement = if reference.is_some() { metrics::text_similarity(&text, ref_text) } else { 0.0 };
                e.metrics.agreement = Some(agreement);
                let mut quality = agreement;
                if let Some(j) = opts.judge {
                    let img = l.rec.image_path.as_deref().map(|p| resolve(p, opts.base)).and_then(|p| std::fs::read(p).ok());
                    match img {
                        Some(bytes) => match j.rate(Arc::from(bytes), &blocks_to_markdown(&r.blocks)).await {
                            Ok(s) => {
                                e.metrics.judge = Some(s);
                                quality = (1.0 - opts.weights.judge) * agreement + opts.weights.judge * s;
                            }
                            Err(err) => tracing::warn!(doc = %l.rec.doc, page = l.rec.page, dag = %l.rec.dag_id, %err, "judge failed"),
                        },
                        None => tracing::warn!(doc = %l.rec.doc, page = l.rec.page, "no page image for the judge"),
                    }
                }
                e.quality = quality.clamp(0.0, 1.0);
            }
            evals.push(e);
        }
    }
    Ok(evals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::{ClassifierTier, DocType, LanguageTag, PageProfile};

    fn profile() -> PageProfile {
        PageProfile {
            language: LanguageTag::parse("en").unwrap(),
            doc_type: DocType::Report,
            born_digital: true,
            text_layer_coverage: 1.0,
            has_handwriting: false,
            has_images: false,
            has_tables: true,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 0.9,
            tier: ClassifierTier::Rules,
        }
    }

    fn draft(kind: BlockKind, b: (f32, f32, f32, f32), order: u32, content: BlockContent, text: &str) -> BlockDraft {
        BlockDraft { kind, bbox: BBox::new(b.0, b.1, b.2, b.3).unwrap(), reading_order: order, content, text: text.into(), level: None, confidence: 0.9, node_id: "n".into() }
    }

    fn rec(dag: &str, ok: bool) -> RunRecord {
        RunRecord {
            doc: "d".into(),
            page: 1,
            dag_id: dag.into(),
            dag_version: "v".into(),
            class_key: "k".into(),
            profile_json: profile(),
            latency_ms: 100.0,
            gpu_seconds: 0.1,
            tokens: 0,
            ok,
            error: None,
            result_path: None,
            image_path: None,
            ground_truth_uri: None,
            split: None,
            blocks: 0,
        }
    }

    #[test]
    fn ground_truth_parses_pixel_and_normalised_boxes() {
        let page: GtPage = serde_json::from_value(json!({
            "width_px": 1000, "height_px": 2000,
            "blocks": [
                {"kind": "text_block", "bbox": [0, 0, 500, 200], "text": "hello", "order": 0},
                {"category": "table", "poly": [0, 600, 500, 600, 500, 1200, 0, 1200], "html": "<table><tr><td>1</td></tr></table>", "order": 1},
                {"kind": "formula", "bbox": {"x0": 0.0, "y0": 0.7, "x1": 0.5, "y1": 0.8}, "latex": "a^2"}
            ]
        }))
        .unwrap();
        let bs = gt_blocks(&page);
        assert_eq!(bs.len(), 3);
        assert_eq!(bs[0].kind, BlockKind::Paragraph);
        assert!((bs[0].bbox.x1 - 0.5).abs() < 1e-6 && (bs[0].bbox.y1 - 0.1).abs() < 1e-6);
        assert_eq!(bs[1].kind, BlockKind::Table);
        assert!((bs[1].bbox.y0 - 0.3).abs() < 1e-6);
        assert_eq!((bs[2].kind, bs[2].order, bs[2].text.as_str()), (BlockKind::Formula, 2, "a^2"));
        assert_eq!(gt_kind("Section-Header"), BlockKind::Heading);
    }

    #[test]
    fn labelled_quality_perfect_and_partial() {
        let gt_page: GtPage = serde_json::from_value(json!({"blocks": [
            {"kind": "heading", "bbox": [0.0, 0.0, 1.0, 0.1], "text": "Title", "order": 0},
            {"kind": "paragraph", "bbox": [0.0, 0.2, 1.0, 0.5], "text": "Body text here", "order": 1},
            {"kind": "table", "bbox": [0.0, 0.6, 1.0, 0.9], "html": "<table><tr><td>1</td><td>2</td></tr></table>", "order": 2}
        ]}))
        .unwrap();
        let gt = gt_blocks(&gt_page);
        let perfect = vec![
            draft(BlockKind::Heading, (0.0, 0.0, 1.0, 0.1), 0, BlockContent::Text { text: "Title".into() }, "Title"),
            draft(BlockKind::Paragraph, (0.0, 0.2, 1.0, 0.5), 1, BlockContent::Text { text: "Body text here".into() }, "Body text here"),
            draft(BlockKind::Table, (0.0, 0.6, 1.0, 0.9), 2, BlockContent::Table { html: "<table><tr><td>1</td><td>2</td></tr></table>".into() }, ""),
        ];
        let m = labelled_metrics(&draft_blocks(&perfect), &gt);
        assert_eq!(m.text, Some(1.0));
        assert_eq!(m.tables, Some(1.0));
        assert_eq!(m.formulas, None);
        assert_eq!(m.reading_order, Some(1.0));
        assert_eq!(m.layout, Some(1.0));
        assert!((quality_from_metrics(&m, &MetricWeights::default()) - 1.0).abs() < 1e-6);

        let partial = vec![perfect[0].clone(), perfect[1].clone()]; // table missing
        let m2 = labelled_metrics(&draft_blocks(&partial), &gt);
        assert_eq!(m2.tables, Some(0.0));
        assert!(m2.layout.unwrap() < 1.0);
        let q = quality_from_metrics(&m2, &MetricWeights::default());
        assert!(q > 0.5 && q < 1.0, "{q}");
        // weights steer the mean
        let w = MetricWeights { tables: 100.0, ..Default::default() };
        assert!(quality_from_metrics(&m2, &w) < 0.1);
    }

    #[test]
    fn failed_run_scores_zero() {
        let e = score_labelled(&rec("x", false), None, &[], &MetricWeights::default());
        assert_eq!(e.quality, 0.0);
        assert!(e.failed);
        assert_eq!(e.page_id, "d/1");
    }

    #[test]
    fn reference_prefers_labelled_then_medoid() {
        let mut lm = HashMap::new();
        lm.insert("b".to_string(), 0.9);
        lm.insert("a".to_string(), 0.5);
        let ok = vec![("a", "hello world"), ("b", "hello world"), ("c", "zzz")];
        assert_eq!(pick_reference(&ok, &lm), Some("b"));
        // medoid: a and b agree, c is the outlier → a (ties → lower id)
        assert_eq!(pick_reference(&ok, &HashMap::new()), Some("a"));
        assert_eq!(pick_reference(&[], &HashMap::new()), None);
    }

    #[test]
    fn markdown_rendering() {
        let ds = vec![
            draft(BlockKind::Paragraph, (0.0, 0.2, 1.0, 0.5), 1, BlockContent::Text { text: "Body".into() }, "Body"),
            draft(BlockKind::Title, (0.0, 0.0, 1.0, 0.1), 0, BlockContent::Text { text: "T".into() }, "T"),
            draft(BlockKind::Formula, (0.0, 0.6, 1.0, 0.7), 2, BlockContent::Formula { latex: "x".into() }, "x"),
        ];
        assert_eq!(blocks_to_markdown(&ds), "# T\n\nBody\n\n$$\nx\n$$\n\n");
    }

    #[tokio::test]
    async fn score_unlabelled_uses_agreement() {
        let dir = std::env::temp_dir().join(format!("vs-adapt-score-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |dag: &str, text: &str| {
            let r = AdaptPageResult {
                doc: "d".into(),
                page_no: 1,
                dag_ref: format!("{dag}@v"),
                profile: profile(),
                render_width_px: 100,
                render_height_px: 100,
                blocks: vec![draft(BlockKind::Paragraph, (0.0, 0.0, 1.0, 0.5), 0, BlockContent::Text { text: text.into() }, text)],
                traces: vec![],
            };
            let p = dir.join(format!("{dag}.json"));
            std::fs::write(&p, serde_json::to_string(&r).unwrap()).unwrap();
            p.to_string_lossy().to_string()
        };
        let mut a = rec("a", true);
        a.result_path = Some(write("a", "the quick brown fox"));
        let mut b = rec("b", true);
        b.result_path = Some(write("b", "the quick brown fox"));
        let mut c = rec("c", true);
        c.result_path = Some(write("c", "the quick brown cat"));
        let f = rec("f", false);
        let evals = score(vec![a, b, c, f], &ScoreOptions { weights: MetricWeights::default(), judge: None, base: None }).await.unwrap();
        assert_eq!(evals.len(), 4);
        assert!(evals.iter().all(|e| e.trust == Trust::Judge));
        let q = |d: &str| evals.iter().find(|e| e.dag_id == d).unwrap().quality;
        assert_eq!(q("a"), 1.0);
        assert_eq!(q("b"), 1.0);
        assert!(q("c") > 0.8 && q("c") < 1.0);
        assert_eq!(q("f"), 0.0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
