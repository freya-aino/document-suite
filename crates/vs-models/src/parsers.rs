//! Output parsers: one per model family. See SPEC.md "Output parsers".
//!
//! Every parser is tolerant: code fences are stripped, JSON is located inside surrounding
//! prose, bboxes are accepted as `[0,1]` fractions, thousandths or pixels of the model input,
//! unknown labels become `BlockKind::Other`, and output that does not match the family's
//! structured format falls back to a Markdown split so no content is lost silently.

use serde_json::Value;

use vs_core::{BBox, BlockContent, BlockKind};
use vs_dag::{BlockDraft, PageImage};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{family}: {reason}")]
    Invalid { family: &'static str, reason: String },
    #[error("no parser for family {0:?}")]
    UnknownFamily(String),
}

pub trait OutputParser: Send + Sync {
    fn family(&self) -> &'static str;
    /// `model_input_size` = (w, h) of the image as the model saw it (after its resizing).
    fn parse(&self, raw: &str, page: &PageImage, model_input_size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError>;
}

pub fn parser_for(family: &str) -> Result<Box<dyn OutputParser>, ParseError> {
    match family {
        "paddleocr_vl" => Ok(Box::new(PaddleOcrVl)),
        "mineru" => Ok(Box::new(MinerU)),
        "dots_ocr" => Ok(Box::new(DotsOcr)),
        "logics_parsing" => Ok(Box::new(LogicsParsing)),
        "generic_json" => Ok(Box::new(GenericJson)),
        other => Err(ParseError::UnknownFamily(other.to_string())),
    }
}

/// Families with a structured page parser (as opposed to e.g. `embedding`).
pub const FAMILIES: &[&str] = &["paddleocr_vl", "mineru", "dots_ocr", "logics_parsing", "generic_json"];

/// Shared label mapping; families map their labels to these names first.
pub fn block_kind_from_label(label: &str) -> BlockKind {
    match label.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
        "title" | "doc_title" | "document_title" => BlockKind::Title,
        "heading" | "section_header" | "section_heading" | "paragraph_title" | "header_1" | "header_2" | "subtitle" => {
            BlockKind::Heading
        }
        "text" | "paragraph" | "plain_text" | "abstract" | "aside_text" | "content" | "body" => BlockKind::Paragraph,
        "list" | "list_item" => BlockKind::List,
        "table" | "table_body" => BlockKind::Table,
        "figure" | "image" | "picture" | "image_body" | "photo" | "illustration" | "diagram" => BlockKind::Figure,
        "chart" | "plot" | "graph" => BlockKind::Chart,
        "formula" | "equation" | "display_formula" | "isolate_formula" | "inline_formula" | "interline_equation"
        | "inline_equation" | "math" | "chemistry" | "chemical_formula" | "chemical" | "smiles" => BlockKind::Formula,
        "caption" | "figure_caption" | "table_caption" | "figure_title" | "table_title" | "chart_title" | "image_caption"
        | "code_caption" => BlockKind::Caption,
        "footnote" | "image_footnote" | "table_footnote" | "page_footnote" | "vision_footnote" => BlockKind::Footnote,
        "header" | "page_header" | "header_image" => BlockKind::Header,
        "footer" | "page_footer" | "footer_image" => BlockKind::Footer,
        "page_number" | "number" => BlockKind::PageNumber,
        "reference" | "references" | "bibliography" | "ref_text" | "citation" => BlockKind::Reference,
        "handwriting" | "handwritten" | "handwritten_text" => BlockKind::Handwriting,
        "seal" | "stamp" => BlockKind::Stamp,
        "code" | "algorithm" | "code_body" => BlockKind::Code,
        _ => BlockKind::Other,
    }
}

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

/// Removes a surrounding Markdown code fence (```json … ```), leading/trailing whitespace,
/// and vLLM/Qwen end-of-text markers.
pub fn strip_code_fences(raw: &str) -> &str {
    let mut s = raw.trim();
    for marker in ["<|im_end|>", "<|endoftext|>", "<|end|>"] {
        if let Some(stripped) = s.strip_suffix(marker) {
            s = stripped.trim_end();
        }
    }
    if let Some(rest) = s.strip_prefix("```") {
        // Drop the info string (e.g. `json`, `html`) up to the first newline.
        let body = match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        let body = body.trim_end();
        s = body.strip_suffix("```").unwrap_or(body).trim();
    }
    s
}

/// Parses `raw` as JSON, or the first balanced `{…}` / `[…]` region inside it.
pub fn extract_json(raw: &str) -> Option<Value> {
    let s = strip_code_fences(raw);
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return Some(v);
    }
    let start = s.find(['{', '['])?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            match b {
                b'\\' if !esc => esc = true,
                b'"' if !esc => in_str = false,
                _ => esc = false,
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&s[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// Reads 4 (x0,y0,x1,y1) or 8 (polygon) numbers from a JSON array or a "x y x y" string.
fn coords(v: &Value) -> Option<[f32; 4]> {
    let nums: Vec<f32> = match v {
        Value::Array(a) => a
            .iter()
            .flat_map(|x| match x {
                Value::Array(inner) => inner.iter().filter_map(|n| n.as_f64()).map(|n| n as f32).collect::<Vec<_>>(),
                other => other.as_f64().map(|n| vec![n as f32]).unwrap_or_default(),
            })
            .collect(),
        Value::String(s) => coords_from_str(s)?,
        _ => return None,
    };
    coords_from_nums(&nums)
}

fn coords_from_str(s: &str) -> Option<Vec<f32>> {
    let nums: Vec<f32> = s
        .split(|c: char| c == ',' || c.is_whitespace() || c == ';')
        .filter(|t| !t.is_empty())
        .map(|t| t.trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')').parse::<f32>())
        .collect::<Result<_, _>>()
        .ok()?;
    Some(nums)
}

fn coords_from_nums(nums: &[f32]) -> Option<[f32; 4]> {
    match nums.len() {
        4 => Some([nums[0], nums[1], nums[2], nums[3]]),
        n if n >= 6 && n % 2 == 0 => {
            let xs = nums.iter().step_by(2);
            let ys = nums.iter().skip(1).step_by(2);
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for (&x, &y) in xs.zip(ys) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
            Some([x0, y0, x1, y1])
        }
        _ => None,
    }
}

/// How a family reports coordinates when they are not already `[0,1]` fractions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordScale {
    /// Pixels of the model input image.
    Pixels,
    /// Thousandths of the image size (0..1000), as MinerU and Logics-Parsing emit. Values
    /// above 1000 are treated as pixels.
    Thousandths,
}

/// Normalises raw coordinates to a `[0,1]` `BBox`; degenerate boxes are clamped and
/// re-ordered rather than rejected.
pub fn normalise_bbox(c: [f32; 4], model_input_size: (u32, u32), scale: CoordScale) -> BBox {
    let c = c.map(|v| if v.is_finite() { v } else { 0.0 });
    let max = c.iter().cloned().fold(0.0f32, f32::max);
    let (sx, sy) = if max <= 1.0 {
        (1.0, 1.0)
    } else if scale == CoordScale::Thousandths && max <= 1000.0 {
        (1000.0, 1000.0)
    } else {
        (model_input_size.0.max(1) as f32, model_input_size.1.max(1) as f32)
    };
    let clamp = |v: f32| v.clamp(0.0, 1.0);
    let (x0, x1) = (clamp(c[0] / sx), clamp(c[2] / sx));
    let (y0, y1) = (clamp(c[1] / sy), clamp(c[3] / sy));
    BBox { x0: x0.min(x1), y0: y0.min(y1), x1: x0.max(x1), y1: y0.max(y1) }
}

pub fn full_page_bbox() -> BBox {
    BBox { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }
}

/// Decodes the common HTML entities.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "dagger" => Some('\u{2020}'),
            "Dagger" => Some('\u{2021}'),
            "ndash" => Some('\u{2013}'),
            "mdash" => Some('\u{2014}'),
            "times" => Some('\u{00d7}'),
            "deg" => Some('\u{00b0}'),
            "plusmn" => Some('\u{00b1}'),
            "micro" => Some('\u{00b5}'),
            "middot" => Some('\u{00b7}'),
            "hellip" => Some('\u{2026}'),
            "lsquo" => Some('\u{2018}'),
            "rsquo" => Some('\u{2019}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            "copy" => Some('\u{00a9}'),
            "reg" => Some('\u{00ae}'),
            "euro" => Some('\u{20ac}'),
            _ => ent
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix('x').map_or_else(|| n.parse::<u32>().ok(), |h| u32::from_str_radix(h, 16).ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Plain-text projection of an HTML fragment: cells separated by tabs, rows/blocks by
/// newlines, tags removed, entities decoded, whitespace collapsed.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find('>') else {
            out.push_str(rest);
            rest = "";
            break;
        };
        let tag = rest[1..end].trim().trim_start_matches('/').to_ascii_lowercase();
        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let closing = rest[1..].starts_with('/');
        match name {
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table" | "thead" | "tbody"
                if closing || name == "br" =>
            {
                out.push('\n')
            }
            "td" | "th" if closing => out.push('\t'),
            _ => {}
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    let decoded = decode_entities(&out);
    decoded
        .lines()
        .map(|l| l.split(['\t']).map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\t"))
        .map(|l| l.trim_matches('\t').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts the first `<table>…</table>` of `s` (case-insensitive), if any.
pub fn extract_table_html(s: &str) -> Option<&str> {
    let lower = s.to_ascii_lowercase();
    let start = lower.find("<table")?;
    let end = lower[start..].find("</table>").map(|e| start + e + "</table>".len()).unwrap_or(s.len());
    Some(&s[start..end])
}

/// Converts a GitHub-style Markdown table to HTML. Returns `None` if `s` is not one.
pub fn markdown_table_to_html(s: &str) -> Option<String> {
    let rows: Vec<Vec<String>> = s
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('|') && l.ends_with('|') && l.len() >= 2)
        .map(|l| l[1..l.len() - 1].split('|').map(|c| c.trim().to_string()).collect())
        .collect();
    if rows.len() < 2 {
        return None;
    }
    let is_sep = |r: &Vec<String>| r.iter().all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'));
    let mut html = String::from("<table>");
    let mut header_done = false;
    for (i, row) in rows.iter().enumerate() {
        if is_sep(row) {
            continue;
        }
        let tag = if i == 0 && rows.get(1).is_some_and(is_sep) && !header_done {
            header_done = true;
            "th"
        } else {
            "td"
        };
        html.push_str("<tr>");
        for cell in row {
            html.push_str(&format!("<{tag}>{}</{tag}>", escape_html(cell)));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    Some(html)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Strips LaTeX display delimiters (`$$…$$`, `\[…\]`, `\(…\)`, `$…$`) and code fences.
pub fn strip_latex_delimiters(s: &str) -> String {
    let mut t = strip_code_fences(s).trim();
    for (open, close) in [("$$", "$$"), ("\\[", "\\]"), ("\\(", "\\)"), ("$", "$")] {
        if let Some(inner) = t.strip_prefix(open).and_then(|x| x.strip_suffix(close)) {
            t = inner.trim();
            break;
        }
    }
    t.to_string()
}

/// Builds `(content, text)` for a block of `kind` from the family's raw content string.
/// Tables become `BlockContent::Table { html }` (Markdown tables are converted), formulas
/// `BlockContent::Formula { latex }`, figures/charts keep any text as description.
pub fn content_for(kind: BlockKind, raw: &str) -> (BlockContent, String) {
    let raw = strip_code_fences(raw);
    match kind {
        BlockKind::Table => {
            let html = match extract_table_html(raw) {
                Some(t) => t.to_string(),
                None => match markdown_table_to_html(raw) {
                    Some(h) => h,
                    None if raw.is_empty() => String::new(),
                    None => format!("<table><tr><td>{}</td></tr></table>", escape_html(raw)),
                },
            };
            let text = html_to_text(&html);
            (BlockContent::Table { html }, text)
        }
        BlockKind::Formula => {
            let latex = strip_latex_delimiters(raw);
            (BlockContent::Formula { latex: latex.clone() }, latex)
        }
        BlockKind::Figure => {
            let text = text_of(raw);
            let description = if text.is_empty() { None } else { Some(text.clone()) };
            (BlockContent::Figure { crop_uri: String::new(), description }, text)
        }
        BlockKind::Chart => {
            let data_html = extract_table_html(raw).map(str::to_string);
            let text = text_of(raw);
            let description = if text.is_empty() { None } else { Some(text.clone()) };
            (BlockContent::Chart { crop_uri: String::new(), data_html, description }, text)
        }
        _ => {
            let text = text_of(raw);
            if text.is_empty() {
                (BlockContent::Empty, text)
            } else {
                (BlockContent::Text { text: text.clone() }, text)
            }
        }
    }
}

/// Text projection of raw content that may be plain text, Markdown or HTML.
fn text_of(raw: &str) -> String {
    let t = raw.trim();
    if t.contains('<') && t.contains("</") {
        html_to_text(t)
    } else {
        decode_entities(t)
    }
}

/// Heading level from a Markdown prefix (`## x` → 2), stripping the hashes.
fn markdown_heading(s: &str) -> Option<(u8, &str)> {
    let t = s.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        Some((hashes as u8, t[hashes..].trim()))
    } else {
        None
    }
}

/// Intermediate block before reading order is assigned.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub kind: BlockKind,
    pub bbox: BBox,
    pub raw: String,
    pub level: Option<u8>,
    pub confidence: f32,
    /// Explicit reading order from the model, if any.
    pub order: Option<u32>,
}

impl Parsed {
    fn new(kind: BlockKind, bbox: BBox, raw: impl Into<String>) -> Self {
        Self { kind, bbox, raw: raw.into(), level: None, confidence: 0.8, order: None }
    }
}

/// Assigns reading order (explicit `order` first, otherwise list position) and builds drafts.
pub fn finish(parsed: Vec<Parsed>, node_id: &str) -> Vec<BlockDraft> {
    let mut indexed: Vec<(usize, Parsed)> = parsed.into_iter().enumerate().collect();
    if indexed.iter().any(|(_, p)| p.order.is_some()) {
        indexed.sort_by_key(|(i, p)| (p.order.unwrap_or(u32::MAX), *i));
    }
    indexed
        .into_iter()
        .enumerate()
        .map(|(ro, (_, p))| {
            let (content, text) = content_for(p.kind, &p.raw);
            BlockDraft {
                kind: p.kind,
                bbox: p.bbox,
                reading_order: ro as u32,
                content,
                text,
                level: p.level,
                confidence: p.confidence.clamp(0.0, 1.0),
                node_id: node_id.to_string(),
            }
        })
        .collect()
}

/// Last-resort parse for unstructured output: Markdown / plain text split into blocks with
/// a full-page bbox (`<table>`s, `$$` formulas and images become their own blocks).
pub fn markdown_fallback(raw: &str) -> Vec<Parsed> {
    let text = strip_code_fences(raw);
    let mut out = Vec::new();
    let mut rest = text;
    let lower_all = text.to_ascii_lowercase();
    let mut offset = 0usize;
    // Split out tables and display formulas first; they may contain blank lines.
    loop {
        let lower = &lower_all[offset..];
        let next_table = lower.find("<table");
        let next_formula = rest.find("$$");
        let (start, is_table) = match (next_table, next_formula) {
            (Some(t), Some(f)) if f < t => (f, false),
            (Some(t), _) => (t, true),
            (None, Some(f)) => (f, false),
            (None, None) => break,
        };
        let end = if is_table {
            lower[start..].find("</table>").map(|e| start + e + 8).unwrap_or(rest.len())
        } else {
            rest[start + 2..].find("$$").map(|e| start + 2 + e + 2).unwrap_or(rest.len())
        };
        push_markdown_paragraphs(&rest[..start], &mut out);
        let seg = &rest[start..end];
        let kind = if is_table { BlockKind::Table } else { BlockKind::Formula };
        let mut p = Parsed::new(kind, full_page_bbox(), seg);
        p.confidence = 0.5;
        out.push(p);
        rest = &rest[end..];
        offset += end;
        if rest.is_empty() {
            break;
        }
    }
    push_markdown_paragraphs(rest, &mut out);
    out
}

fn push_markdown_paragraphs(text: &str, out: &mut Vec<Parsed>) {
    for chunk in text.split("\n\n").map(str::trim).filter(|c| !c.is_empty()) {
        let (kind, level, body) = if let Some((lvl, body)) = markdown_heading(chunk) {
            (if lvl == 1 { BlockKind::Title } else { BlockKind::Heading }, Some(lvl), body.to_string())
        } else if chunk.starts_with("![") {
            (BlockKind::Figure, None, chunk.to_string())
        } else if chunk.lines().all(|l| {
            let l = l.trim_start();
            l.starts_with("- ") || l.starts_with("* ") || l.starts_with("• ") || l.split('.').next().is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && l[n.len()..].starts_with(". "))
        }) {
            (BlockKind::List, None, chunk.to_string())
        } else if chunk.lines().all(|l| l.trim().starts_with('|')) && chunk.lines().count() >= 2 {
            (BlockKind::Table, None, chunk.to_string())
        } else {
            (BlockKind::Paragraph, None, chunk.to_string())
        };
        let mut p = Parsed::new(kind, full_page_bbox(), body);
        p.level = level;
        p.confidence = 0.5;
        out.push(p);
    }
}

fn str_field<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| item.get(k).and_then(Value::as_str))
}

fn num_field(item: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| item.get(k).and_then(Value::as_f64))
}

fn value_field<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| item.get(k)).filter(|v| !v.is_null())
}

/// Content from a JSON value: strings as-is, string arrays joined by newlines, objects by their
/// text-ish fields, anything else serialised (never dropped).
fn content_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(content_string).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        Value::Null => String::new(),
        Value::Object(_) => str_field(v, &["html", "latex", "text", "content", "markdown"])
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    }
}

fn items_of<'a>(v: &'a Value, list_keys: &[&str]) -> Option<&'a Vec<Value>> {
    match v {
        Value::Array(a) => Some(a),
        Value::Object(_) => list_keys.iter().find_map(|k| v.get(k).and_then(Value::as_array)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// PaddleOCR-VL
// ---------------------------------------------------------------------------------------

/// PaddleOCR-VL(-1.5). Page-level output is the pipeline's `parsing_res_list`
/// (`block_label`, `block_bbox` in input pixels, `block_content`, optional `block_order`) or
/// a bare list of `{label, bbox, content}`; region-level output is plain text / HTML / LaTeX.
pub struct PaddleOcrVl;

impl OutputParser for PaddleOcrVl {
    fn family(&self) -> &'static str {
        "paddleocr_vl"
    }
    fn parse(&self, raw: &str, _page: &PageImage, size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError> {
        let parsed = match extract_json(raw) {
            Some(v) => {
                let v = v.get("layout_parsing_result").cloned().unwrap_or(v);
                let items = items_of(&v, &["parsing_res_list", "blocks", "layout", "result", "regions"])
                    .ok_or_else(|| ParseError::Invalid { family: "paddleocr_vl", reason: "JSON has no block list".into() })?;
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let label = str_field(item, &["block_label", "label", "type", "category"]).unwrap_or("other");
                    let kind = block_kind_from_label(label);
                    let bbox = value_field(item, &["block_bbox", "bbox", "box", "coordinate"])
                        .and_then(coords)
                        .map(|c| normalise_bbox(c, size, CoordScale::Pixels))
                        .unwrap_or_else(full_page_bbox);
                    let content = value_field(item, &["block_content", "content", "text", "html", "latex"])
                        .map(content_string)
                        .unwrap_or_default();
                    let mut p = Parsed::new(kind, bbox, content);
                    p.order = num_field(item, &["block_order", "order", "index", "reading_order"]).map(|o| o.max(0.0) as u32);
                    p.confidence = num_field(item, &["score", "confidence"]).map(|s| s as f32).unwrap_or(0.8);
                    if kind == BlockKind::Heading {
                        p.level = Some(if label.eq_ignore_ascii_case("paragraph_title") { 2 } else { 1 });
                    }
                    out.push(p);
                }
                out
            }
            None => markdown_fallback(raw),
        };
        Ok(finish(parsed, node_id))
    }
}

// ---------------------------------------------------------------------------------------
// MinerU
// ---------------------------------------------------------------------------------------

/// MinerU 2.x. Accepts the `content_list` JSON (`type`, `bbox` in thousandths, `text`,
/// `text_level`, `table_body`, `*_caption`, `*_footnote`, `list_items`) or the VLM's raw
/// tagged output `<|box_start|>x0 y0 x1 y1<|box_end|><|ref_start|>type<|ref_end|><|md_start|>…<|md_end|>`.
pub struct MinerU;

impl OutputParser for MinerU {
    fn family(&self) -> &'static str {
        "mineru"
    }
    fn parse(&self, raw: &str, _page: &PageImage, size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError> {
        let parsed = if raw.contains("<|box_start|>") || raw.contains("<|ref_start|>") {
            parse_mineru_tagged(raw, size)
        } else if let Some(v) = extract_json(raw) {
            let items = items_of(&v, &["content_list", "blocks", "para_blocks", "result"])
                .ok_or_else(|| ParseError::Invalid { family: "mineru", reason: "JSON has no content list".into() })?;
            let mut out = Vec::new();
            for item in items {
                mineru_item(item, size, &mut out);
            }
            out
        } else {
            markdown_fallback(raw)
        };
        Ok(finish(parsed, node_id))
    }
}

fn mineru_item(item: &Value, size: (u32, u32), out: &mut Vec<Parsed>) {
    let ty = str_field(item, &["type", "category", "label"]).unwrap_or("other");
    let bbox = value_field(item, &["bbox", "box"])
        .and_then(coords)
        .map(|c| normalise_bbox(c, size, CoordScale::Thousandths))
        .unwrap_or_else(full_page_bbox);
    let level = num_field(item, &["text_level", "level"]).map(|l| l.clamp(1.0, 6.0) as u8);
    let mut kind = block_kind_from_label(ty);
    if kind == BlockKind::Paragraph && level.is_some() {
        kind = BlockKind::Heading;
    }
    let main = match kind {
        BlockKind::Table => value_field(item, &["table_body", "html", "text"]).map(content_string).unwrap_or_default(),
        BlockKind::Formula => value_field(item, &["latex", "text", "content"]).map(content_string).unwrap_or_default(),
        BlockKind::Code => value_field(item, &["code_body", "text", "content"]).map(content_string).unwrap_or_default(),
        BlockKind::List => value_field(item, &["list_items", "text", "content"]).map(content_string).unwrap_or_default(),
        BlockKind::Figure => {
            // Figures carry no text of their own; keep the image path as a hint in the description
            // only if there is nothing better (captions become separate blocks below).
            value_field(item, &["text", "content"]).map(content_string).unwrap_or_default()
        }
        _ => value_field(item, &["text", "content", "html"]).map(content_string).unwrap_or_default(),
    };
    let mut p = Parsed::new(kind, bbox, main);
    p.level = if kind == BlockKind::Heading { level.or(Some(1)) } else { None };
    p.order = num_field(item, &["reading_order", "order", "index"]).map(|o| o.max(0.0) as u32);
    p.confidence = num_field(item, &["score", "confidence"]).map(|s| s as f32).unwrap_or(0.8);
    let confidence = p.confidence;
    out.push(p);
    // Attached captions / footnotes are their own blocks (same bbox, directly after the parent).
    for (key, k) in [
        ("table_caption", BlockKind::Caption),
        ("image_caption", BlockKind::Caption),
        ("img_caption", BlockKind::Caption),
        ("code_caption", BlockKind::Caption),
        ("table_footnote", BlockKind::Footnote),
        ("image_footnote", BlockKind::Footnote),
        ("img_footnote", BlockKind::Footnote),
    ] {
        if let Some(v) = value_field(item, &[key]) {
            let s = content_string(v);
            if !s.trim().is_empty() {
                let mut c = Parsed::new(k, bbox, s);
                c.confidence = confidence;
                out.push(c);
            }
        }
    }
}

fn parse_mineru_tagged(raw: &str, size: (u32, u32)) -> Vec<Parsed> {
    const BOX_S: &str = "<|box_start|>";
    const BOX_E: &str = "<|box_end|>";
    const REF_S: &str = "<|ref_start|>";
    const REF_E: &str = "<|ref_end|>";
    const MD_S: &str = "<|md_start|>";
    const MD_E: &str = "<|md_end|>";
    let mut out = Vec::new();
    let mut rest = strip_code_fences(raw);
    while let Some(bs) = rest.find(BOX_S) {
        let after_box = &rest[bs + BOX_S.len()..];
        let Some(be) = after_box.find(BOX_E) else { break };
        let bbox = coords_from_str(&after_box[..be])
            .and_then(|n| coords_from_nums(&n))
            .map(|c| normalise_bbox(c, size, CoordScale::Thousandths))
            .unwrap_or_else(full_page_bbox);
        let mut tail = &after_box[be + BOX_E.len()..];
        let mut label = "other";
        if let Some(t) = tail.trim_start().strip_prefix(REF_S) {
            if let Some(re) = t.find(REF_E) {
                label = t[..re].trim();
                tail = &t[re + REF_E.len()..];
            }
        }
        let mut content = "";
        let next_block = tail.find(BOX_S).unwrap_or(tail.len());
        if let Some(t) = tail.trim_start().strip_prefix(MD_S) {
            let lead = tail.len() - tail.trim_start().len() + MD_S.len();
            let me = t.find(MD_E).unwrap_or(t.len());
            content = t[..me].trim();
            let consumed = lead + me + if t[me..].starts_with(MD_E) { MD_E.len() } else { 0 };
            rest = &tail[consumed.min(tail.len())..];
        } else {
            rest = &tail[next_block..];
        }
        let (kind, level) = mineru_label(label);
        let mut p = Parsed::new(kind, bbox, content);
        p.level = level;
        out.push(p);
    }
    if out.is_empty() {
        return markdown_fallback(raw);
    }
    out
}

fn mineru_label(label: &str) -> (BlockKind, Option<u8>) {
    let l = label.to_ascii_lowercase();
    let kind = block_kind_from_label(&l);
    match kind {
        BlockKind::Title => (BlockKind::Heading, Some(1)),
        BlockKind::Heading => (kind, Some(2)),
        _ => (kind, None),
    }
}

// ---------------------------------------------------------------------------------------
// dots.ocr
// ---------------------------------------------------------------------------------------

/// dots.ocr: JSON array of `{bbox: [x0,y0,x1,y1] (input pixels), category, text}` where
/// `Table` text is HTML, `Formula` text is LaTeX and `Picture` has no text.
pub struct DotsOcr;

impl OutputParser for DotsOcr {
    fn family(&self) -> &'static str {
        "dots_ocr"
    }
    fn parse(&self, raw: &str, _page: &PageImage, size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError> {
        let parsed = match extract_json(raw) {
            Some(v) => {
                let items = items_of(&v, &["blocks", "layout", "result", "cells"])
                    .ok_or_else(|| ParseError::Invalid { family: "dots_ocr", reason: "JSON has no block list".into() })?;
                items
                    .iter()
                    .map(|item| {
                        let label = str_field(item, &["category", "label", "type"]).unwrap_or("other");
                        let kind = block_kind_from_label(label);
                        let bbox = value_field(item, &["bbox", "box"])
                            .and_then(coords)
                            .map(|c| normalise_bbox(c, size, CoordScale::Pixels))
                            .unwrap_or_else(full_page_bbox);
                        let content = value_field(item, &["text", "content", "html", "latex"]).map(content_string).unwrap_or_default();
                        let mut p = Parsed::new(kind, bbox, content);
                        p.order = num_field(item, &["reading_order", "order", "index"]).map(|o| o.max(0.0) as u32);
                        p.confidence = num_field(item, &["score", "confidence"]).map(|s| s as f32).unwrap_or(0.8);
                        if kind == BlockKind::Heading {
                            p.level = Some(1);
                        }
                        p
                    })
                    .collect()
            }
            None => markdown_fallback(raw),
        };
        Ok(finish(parsed, node_id))
    }
}

// ---------------------------------------------------------------------------------------
// Logics-Parsing
// ---------------------------------------------------------------------------------------

/// Logics-Parsing: HTML with one `<div class="<category>" data-bbox="x0 y0 x1 y1">` per
/// block (coordinates in thousandths; pixels accepted). Tables keep their inner `<table>`,
/// formulas their LaTeX, everything else is projected to text.
pub struct LogicsParsing;

impl OutputParser for LogicsParsing {
    fn family(&self) -> &'static str {
        "logics_parsing"
    }
    fn parse(&self, raw: &str, _page: &PageImage, size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError> {
        let text = strip_code_fences(raw);
        let mut parsed = Vec::new();
        for el in html_elements(text) {
            let Some(bbox_attr) = el.attr("data-bbox").or_else(|| el.attr("bbox")) else { continue };
            let bbox = coords_from_str(bbox_attr)
                .and_then(|n| coords_from_nums(&n))
                .map(|c| normalise_bbox(c, size, CoordScale::Thousandths))
                .unwrap_or_else(full_page_bbox);
            let class = el.attr("class").or_else(|| el.attr("data-category")).unwrap_or("other").to_string();
            let primary = class.split_whitespace().next().unwrap_or("other");
            let mut kind = block_kind_from_label(primary);
            let inner = el.inner.trim();
            let mut level = None;
            if matches!(kind, BlockKind::Title | BlockKind::Heading) {
                let lvl = heading_level_in_html(inner);
                match (kind, lvl) {
                    (BlockKind::Title, Some(l)) if l >= 2 => {
                        kind = BlockKind::Heading;
                        level = Some(l);
                    }
                    (BlockKind::Heading, l) => level = l.or(Some(1)),
                    _ => {}
                }
            }
            let content = match kind {
                BlockKind::Table | BlockKind::Formula | BlockKind::Chart => inner.to_string(),
                BlockKind::Figure => {
                    // Keep alt text / caption text inside the figure div if any.
                    let t = html_to_text(inner);
                    if t.is_empty() { img_alt(inner).unwrap_or_default() } else { t }
                }
                BlockKind::Code => html_to_text(inner),
                _ => html_to_text(inner),
            };
            let mut p = Parsed::new(kind, bbox, content);
            p.level = level;
            p.order = el.attr("data-order").and_then(|o| o.parse::<u32>().ok());
            p.confidence = el.attr("data-score").and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.8);
            parsed.push(p);
        }
        if parsed.is_empty() {
            // No bbox-tagged blocks: fall back on the text projection so nothing is lost.
            let fallback_src = if text.contains('<') { html_to_text(text) } else { text.to_string() };
            parsed = markdown_fallback(&fallback_src);
        }
        Ok(finish(parsed, node_id))
    }
}

fn heading_level_in_html(s: &str) -> Option<u8> {
    let lower = s.to_ascii_lowercase();
    (1..=6u8).find(|l| lower.contains(&format!("<h{l}")))
}

fn img_alt(s: &str) -> Option<String> {
    let i = s.to_ascii_lowercase().find("alt=\"")?;
    let rest = &s[i + 5..];
    let end = rest.find('"')?;
    Some(decode_entities(&rest[..end]))
}

/// A top-level HTML element with its attributes and inner HTML.
struct HtmlElement<'a> {
    attrs: Vec<(String, &'a str)>,
    inner: &'a str,
}

impl<'a> HtmlElement<'a> {
    fn attr(&self, name: &str) -> Option<&'a str> {
        self.attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
    }
}

/// Lists the top-level block elements (`div`, `p`, `section`, `table`, `h1`..`h6`, `figure`,
/// `pre`) of an HTML fragment, matching nested elements of the same tag name by depth.
fn html_elements(s: &str) -> Vec<HtmlElement<'_>> {
    const BLOCK_TAGS: &[&str] = &["div", "p", "section", "table", "h1", "h2", "h3", "h4", "h5", "h6", "figure", "pre", "span"];
    let mut out = Vec::new();
    let lower = s.to_ascii_lowercase();
    let mut pos = 0usize;
    while let Some(i) = lower[pos..].find('<') {
        let start = pos + i;
        let Some(close) = lower[start..].find('>') else { break };
        let tag_end = start + close;
        let tag_src = &s[start + 1..tag_end];
        let tag_name = tag_src.split(|c: char| c.is_whitespace() || c == '/' || c == '>').next().unwrap_or("").to_ascii_lowercase();
        if tag_name.is_empty() || tag_name.starts_with('!') || tag_name.starts_with('/') || !BLOCK_TAGS.contains(&tag_name.as_str()) {
            pos = tag_end + 1;
            continue;
        }
        let attrs = parse_attrs(tag_src);
        if tag_src.trim_end().ends_with('/') {
            out.push(HtmlElement { attrs, inner: "" });
            pos = tag_end + 1;
            continue;
        }
        // Find the matching close tag at depth 0.
        let open_pat = format!("<{tag_name}");
        let close_pat = format!("</{tag_name}");
        let mut depth = 1i32;
        let mut cursor = tag_end + 1;
        let mut inner_end = s.len();
        let mut next_pos = s.len();
        while cursor < lower.len() {
            let no = lower[cursor..].find(&open_pat).map(|x| cursor + x);
            let nc = lower[cursor..].find(&close_pat).map(|x| cursor + x);
            match (no, nc) {
                (Some(o), Some(c)) if o < c && is_tag_boundary(&lower, o + open_pat.len()) => {
                    depth += 1;
                    cursor = o + open_pat.len();
                }
                (Some(o), Some(c)) if o < c => cursor = o + open_pat.len(),
                (_, Some(c)) => {
                    depth -= 1;
                    if depth == 0 {
                        inner_end = c;
                        next_pos = lower[c..].find('>').map(|x| c + x + 1).unwrap_or(s.len());
                        break;
                    }
                    cursor = c + close_pat.len();
                }
                (Some(o), None) => cursor = o + open_pat.len(),
                (None, None) => break,
            }
        }
        out.push(HtmlElement { attrs, inner: &s[tag_end + 1..inner_end] });
        pos = next_pos;
    }
    out
}

fn is_tag_boundary(lower: &str, idx: usize) -> bool {
    lower[idx..].chars().next().map_or(true, |c| c.is_whitespace() || c == '>' || c == '/')
}

fn parse_attrs(tag_src: &str) -> Vec<(String, &str)> {
    let mut attrs = Vec::new();
    let rest = match tag_src.find(|c: char| c.is_whitespace()) {
        Some(i) => &tag_src[i..],
        None => return attrs,
    };
    let mut r = rest;
    loop {
        r = r.trim_start();
        if r.is_empty() || r.starts_with('/') {
            break;
        }
        let name_end = r.find(|c: char| c == '=' || c.is_whitespace() || c == '/').unwrap_or(r.len());
        let name = r[..name_end].to_ascii_lowercase();
        r = &r[name_end..];
        let r2 = r.trim_start();
        if let Some(after_eq) = r2.strip_prefix('=') {
            let v = after_eq.trim_start();
            if let Some(q) = v.chars().next().filter(|&c| c == '"' || c == '\'') {
                let body = &v[1..];
                let end = body.find(q).unwrap_or(body.len());
                attrs.push((name, &body[..end]));
                r = &body[(end + 1).min(body.len())..];
            } else {
                let end = v.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(v.len());
                attrs.push((name, &v[..end]));
                r = &v[end..];
            }
        } else {
            if !name.is_empty() {
                attrs.push((name, ""));
            }
            r = r2;
        }
    }
    attrs
}

// ---------------------------------------------------------------------------------------
// Generic JSON (our own schema, used with guided decoding on general VLMs)
// ---------------------------------------------------------------------------------------

/// Our own schema: `{"blocks":[{"kind","bbox":[x0,y0,x1,y1],"text","html"?,"latex"?,
/// "level"?,"confidence"?,"reading_order"?}]}` (a bare array or a `regions` list are also
/// accepted; bbox as fractions or model-input pixels).
pub struct GenericJson;

impl OutputParser for GenericJson {
    fn family(&self) -> &'static str {
        "generic_json"
    }
    fn parse(&self, raw: &str, _page: &PageImage, size: (u32, u32), node_id: &str) -> Result<Vec<BlockDraft>, ParseError> {
        let parsed = match extract_json(raw) {
            Some(v) => {
                let items = items_of(&v, &["blocks", "regions", "elements", "layout", "result"])
                    .ok_or_else(|| ParseError::Invalid { family: "generic_json", reason: "JSON has no `blocks` list".into() })?;
                items
                    .iter()
                    .map(|item| {
                        let label = str_field(item, &["kind", "label", "category", "type"]).unwrap_or("other");
                        let kind = block_kind_from_label(label);
                        let bbox = value_field(item, &["bbox", "box"])
                            .and_then(coords)
                            .map(|c| normalise_bbox(c, size, CoordScale::Pixels))
                            .unwrap_or_else(full_page_bbox);
                        let content = match kind {
                            BlockKind::Table => value_field(item, &["html", "table_html", "text", "content"]),
                            BlockKind::Formula => value_field(item, &["latex", "text", "content"]),
                            _ => value_field(item, &["text", "content", "html", "latex", "description"]),
                        }
                        .map(content_string)
                        .unwrap_or_default();
                        let mut p = Parsed::new(kind, bbox, content);
                        p.level = num_field(item, &["level"]).map(|l| l.clamp(1.0, 6.0) as u8);
                        p.order = num_field(item, &["reading_order", "order"]).map(|o| o.max(0.0) as u32);
                        p.confidence = num_field(item, &["confidence", "score"]).map(|s| s as f32).unwrap_or(0.8);
                        p
                    })
                    .collect()
            }
            None => markdown_fallback(raw),
        };
        Ok(finish(parsed, node_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_mapping() {
        assert_eq!(block_kind_from_label("Section-Header"), BlockKind::Heading);
        assert_eq!(block_kind_from_label("seal"), BlockKind::Stamp);
        assert_eq!(block_kind_from_label("weird"), BlockKind::Other);
        assert_eq!(block_kind_from_label("List-item"), BlockKind::List);
        assert_eq!(block_kind_from_label("Picture"), BlockKind::Figure);
    }

    #[test]
    fn strips_fences_and_locates_json() {
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```\n[1]\n```<|im_end|>"), "[1]");
        let v = extract_json("Here is the result:\n[{\"a\":\"x]y\"}]\nDone.").unwrap();
        assert_eq!(v[0]["a"], "x]y");
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn bbox_scales() {
        let b = normalise_bbox([0.1, 0.2, 0.5, 0.6], (1000, 2000), CoordScale::Pixels);
        assert!((b.x0 - 0.1).abs() < 1e-6 && (b.y1 - 0.6).abs() < 1e-6);
        let b = normalise_bbox([100.0, 200.0, 500.0, 1200.0], (1000, 2000), CoordScale::Pixels);
        assert!((b.x0 - 0.1).abs() < 1e-6 && (b.y1 - 0.6).abs() < 1e-6);
        let b = normalise_bbox([100.0, 200.0, 500.0, 600.0], (1000, 2000), CoordScale::Thousandths);
        assert!((b.y1 - 0.6).abs() < 1e-6);
        // Out of range and reversed corners are repaired, not rejected.
        let b = normalise_bbox([1200.0, -5.0, 100.0, 50.0], (1000, 100), CoordScale::Pixels);
        assert_eq!((b.x0, b.x1, b.y0, b.y1), (0.1, 1.0, 0.0, 0.5));
        // Polygon with 8 coordinates.
        let c = coords(&serde_json::json!([[10, 10], [90, 12], [92, 50], [8, 48]])).unwrap();
        assert_eq!(c, [8.0, 10.0, 92.0, 50.0]);
    }

    #[test]
    fn html_projection_and_tables() {
        let html = "<table><tr><th>A</th><th>B&amp;C</th></tr><tr><td>1</td><td>2</td></tr></table>";
        assert_eq!(html_to_text(html), "A\tB&C\n1\t2");
        let md = "| h1 | h2 |\n|---|---|\n| a | b |";
        let h = markdown_table_to_html(md).unwrap();
        assert!(h.starts_with("<table><tr><th>h1</th>"));
        let (c, t) = content_for(BlockKind::Table, md);
        assert!(matches!(c, BlockContent::Table { .. }));
        assert_eq!(t, "h1\th2\na\tb");
        let (c, _) = content_for(BlockKind::Formula, "$$E = mc^2$$");
        assert_eq!(c, BlockContent::Formula { latex: "E = mc^2".into() });
    }

    #[test]
    fn fallback_keeps_everything() {
        let p = markdown_fallback("# Title\n\nSome text.\n\n<table><tr><td>x</td></tr></table>\n\n$$a+b$$\n\n- one\n- two");
        let kinds: Vec<BlockKind> = p.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![BlockKind::Title, BlockKind::Paragraph, BlockKind::Table, BlockKind::Formula, BlockKind::List]);
        assert_eq!(p[0].raw, "Title");
    }

    #[test]
    fn html_element_scanner() {
        let els = html_elements(r#"<div class="text" data-bbox="1 2 3 4">a<div>nested</div>b</div><div class="table" data-bbox="5 6 7 8"><table><tr><td>x</td></tr></table></div>"#);
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].attr("class"), Some("text"));
        assert_eq!(els[0].inner, "a<div>nested</div>b");
        assert_eq!(els[1].attr("data-bbox"), Some("5 6 7 8"));
    }
}
