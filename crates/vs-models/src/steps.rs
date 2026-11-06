//! Model-backed DAG steps. See SPEC.md "Model-backed steps".

use async_trait::async_trait;
use futures::StreamExt;
use image::{DynamicImage, GenericImageView, ImageFormat};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::sync::Arc;

use vs_core::{BBox, BlockContent, BlockKind};
use vs_dag::{Artifact, ArtifactKind, BlockDraft, LayoutRegion, PageImage, Signature, Step, StepContext, StepError, StepOutput};

use crate::client::{ChatRequest, OpenAiClient};
use crate::parsers::{block_kind_from_label, content_for, extract_json, parser_for, OutputParser};
use crate::prompts::{self, Prompt};

// ---------------------------------------------------------------------------------------
// Param helpers
// ---------------------------------------------------------------------------------------

fn model_param(params: &Value) -> Result<&str, StepError> {
    params["model"].as_str().ok_or_else(|| StepError::InvalidParams("`model` is required".into()))
}

fn max_tokens(params: &Value, default: u32) -> u32 {
    params["max_tokens"].as_u64().map(|v| v as u32).unwrap_or(default)
}

/// `params.kinds`: list of block kind names (snake_case or family labels) → kinds.
fn kinds_param(params: &Value) -> Result<Option<Vec<BlockKind>>, StepError> {
    match &params["kinds"] {
        Value::Null => Ok(None),
        Value::Array(a) => a
            .iter()
            .map(|v| {
                let s = v.as_str().ok_or_else(|| StepError::InvalidParams("`kinds` must be strings".into()))?;
                match block_kind_from_label(s) {
                    BlockKind::Other if !s.eq_ignore_ascii_case("other") => {
                        Err(StepError::InvalidParams(format!("unknown block kind {s:?} in `kinds`")))
                    }
                    k => Ok(k),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(StepError::InvalidParams("`kinds` must be an array".into())),
    }
}

/// Resolved prompt text with its provenance reference.
struct ResolvedPrompt {
    text: String,
    reference: String,
}

/// Chooses the prompt for a step: `params.prompt` (a compiled-in prompt id, or literal text if
/// it is not one) → the family's native task prompt → the generic prompt file.
fn resolve_prompt(params: &Value, family: &str, task: &str, generic: &Prompt, language: &str) -> ResolvedPrompt {
    let render = |p: &Prompt| ResolvedPrompt { text: p.render(&[("language", language)]), reference: p.reference() };
    if let Some(override_) = params["prompt"].as_str() {
        if let Some(p) = prompts::by_id(override_) {
            return render(p);
        }
        let text = override_.replace("{language}", language);
        let hash = hex::encode(Sha256::digest(override_.as_bytes()));
        return ResolvedPrompt { text, reference: format!("inline@{}", &hash[..8]) };
    }
    if let Some(p) = prompts::family_task_prompt(family, task) {
        return render(p);
    }
    render(generic)
}

fn parser_or_none(family: &str) -> Option<Box<dyn OutputParser>> {
    parser_for(family).ok()
}

fn model_err(e: impl std::fmt::Display) -> StepError {
    StepError::Model(e.to_string())
}

fn parse_err(family: &str, step: &str, e: impl std::fmt::Display) -> StepError {
    tracing::warn!(target: "vs_models::metrics", family, step, error = %e, "parser_failure");
    StepError::Parse(e.to_string())
}

/// Runs `f` over `items` with at most `limit` futures in flight; results in input order.
async fn run_bounded<T, R, F, Fut>(items: Vec<T>, limit: usize, f: F) -> Vec<R>
where
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let mut results: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    let mut stream = futures::stream::iter(items.into_iter().enumerate())
        .map(|(i, item)| {
            let fut = f(item);
            async move { (i, fut.await) }
        })
        .buffer_unordered(limit.max(1));
    while let Some((i, r)) = stream.next().await {
        results[i] = Some(r);
    }
    results.into_iter().map(|r| r.expect("every index filled")).collect()
}

// ---------------------------------------------------------------------------------------
// Cropping
// ---------------------------------------------------------------------------------------

/// Default padding around a region crop, in pixels of the page image.
const CROP_PADDING_PX: u32 = 4;

/// A region crop encoded as PNG.
pub struct RegionCrop {
    pub region: LayoutRegion,
    pub png: Arc<[u8]>,
    pub width_px: u32,
    pub height_px: u32,
}

pub fn decode_page(page: &PageImage) -> Result<DynamicImage, StepError> {
    image::load_from_memory(&page.bytes).map_err(|e| StepError::Io(format!("decode page image: {e}")))
}

/// Crops `bbox` (normalised) from `img` with `padding` pixels and encodes it as PNG.
pub fn crop_png(img: &DynamicImage, bbox: &BBox, padding: u32) -> Result<(Vec<u8>, u32, u32), StepError> {
    let (w, h) = img.dimensions();
    let px = |v: f32, size: u32| (v * size as f32).round().clamp(0.0, size as f32) as u32;
    let x0 = px(bbox.x0, w).saturating_sub(padding);
    let y0 = px(bbox.y0, h).saturating_sub(padding);
    let x1 = (px(bbox.x1, w) + padding).min(w).max(x0 + 1).min(w.max(1));
    let y1 = (px(bbox.y1, h) + padding).min(h).max(y0 + 1).min(h.max(1));
    let (cw, ch) = ((x1 - x0).max(1), (y1 - y0).max(1));
    let x0 = x0.min(w.saturating_sub(cw));
    let y0 = y0.min(h.saturating_sub(ch));
    let crop = img.crop_imm(x0, y0, cw, ch);
    let mut out = Cursor::new(Vec::new());
    crop.write_to(&mut out, ImageFormat::Png).map_err(|e| StepError::Io(format!("encode crop: {e}")))?;
    Ok((out.into_inner(), cw, ch))
}

/// Decodes the page once and crops every region (on a blocking thread).
pub async fn crop_regions(page: &PageImage, regions: Vec<LayoutRegion>, padding: u32) -> Result<Vec<RegionCrop>, StepError> {
    let bytes = page.bytes.clone();
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&bytes).map_err(|e| StepError::Io(format!("decode page image: {e}")))?;
        regions
            .into_iter()
            .map(|region| {
                let (png, width_px, height_px) = crop_png(&img, &region.bbox, padding)?;
                Ok(RegionCrop { region, png: Arc::from(png), width_px, height_px })
            })
            .collect::<Result<Vec<_>, StepError>>()
    })
    .await
    .map_err(|e| StepError::Io(format!("crop task: {e}")))?
}

/// Layout regions from parsed blocks (for doc-VLMs used as layout models).
pub fn regions_from_blocks(blocks: &[BlockDraft]) -> Vec<LayoutRegion> {
    let mut regions: Vec<LayoutRegion> = blocks
        .iter()
        .map(|b| LayoutRegion { kind: b.kind, bbox: b.bbox, reading_order: b.reading_order, score: b.confidence })
        .collect();
    regions.sort_by_key(|r| r.reading_order);
    regions
}

/// JSON schema for our generic block list (`generic_json` family), with or without content.
pub fn blocks_schema(with_content: bool) -> Value {
    let mut props = json!({
        "label": {"type": "string"},
        "bbox": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4},
        "reading_order": {"type": "integer", "minimum": 0},
        "score": {"type": "number", "minimum": 0, "maximum": 1}
    });
    let mut required = vec!["label", "bbox", "reading_order", "score"];
    if with_content {
        props["text"] = json!({"type": "string"});
        props["level"] = json!({"type": "integer", "minimum": 0, "maximum": 6});
        required.extend(["text", "level"]);
    }
    let key = if with_content { "blocks" } else { "regions" };
    json!({
        "type": "object",
        "properties": { key: {"type": "array", "items": {"type": "object", "properties": props, "required": required}} },
        "required": [key]
    })
}

// ---------------------------------------------------------------------------------------
// page.parse_e2e
// ---------------------------------------------------------------------------------------

/// `page_image → blocks`: one VLM call per page.
pub struct PageParseE2e {
    pub client: Arc<OpenAiClient>,
}

#[async_trait]
impl Step for PageParseE2e {
    fn kind(&self) -> &'static str {
        "page.parse_e2e"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::PageImage], output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        120_000
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &Value) -> Result<StepOutput, StepError> {
        let Artifact::PageImage(page) = inputs[0] else { return Err(StepError::InvalidParams("expected page_image".into())) };
        let model = model_param(params)?;
        let spec = self.client.spec(model).map_err(model_err)?;
        let parser = parser_for(&spec.family).map_err(|e| StepError::InvalidParams(e.to_string()))?;
        let language = ctx.profile.language.as_str();
        // `task_prompt` is the historical name of the override; `prompt` is the common one.
        let prompt = match params["task_prompt"].as_str() {
            Some(t) if params["prompt"].is_null() => resolve_prompt(&json!({"prompt": t}), &spec.family, "page_parse", &prompts::PAGE_PARSE, language),
            _ => resolve_prompt(params, &spec.family, "page_parse", &prompts::PAGE_PARSE, language),
        };
        let json_schema = (spec.family == "generic_json").then(|| blocks_schema(true));
        tracing::debug!(step = self.kind(), model, node = %ctx.node_id, "model_step");
        let resp = self
            .client
            .chat(model, &ChatRequest {
                prompt: prompt.text,
                images: vec![(page.bytes.clone(), "image/png")],
                max_tokens: max_tokens(params, 8192),
                json_schema,
                ..Default::default()
            })
            .await
            .map_err(model_err)?;
        let blocks = parser
            .parse(&resp.text, page, (page.width_px, page.height_px), &ctx.node_id)
            .map_err(|e| parse_err(&spec.family, self.kind(), e))?;
        tracing::debug!(step = self.kind(), blocks = blocks.len(), "parsed page");
        Ok(StepOutput { artifact: Artifact::Blocks(blocks), model_ref: Some(spec.model_ref()), prompt_ref: Some(prompt.reference) })
    }
}

// ---------------------------------------------------------------------------------------
// layout.detect
// ---------------------------------------------------------------------------------------

/// `page_image → layout`. General VLMs (`generic_json`) answer a JSON-schema-guided region
/// list; document-parsing VLMs are asked with their native layout prompt and their family
/// parser's blocks are reduced to regions.
pub struct LayoutDetect {
    pub client: Arc<OpenAiClient>,
}

#[async_trait]
impl Step for LayoutDetect {
    fn kind(&self) -> &'static str {
        "layout.detect"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::PageImage], output: ArtifactKind::Layout }
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &Value) -> Result<StepOutput, StepError> {
        let Artifact::PageImage(page) = inputs[0] else { return Err(StepError::InvalidParams("expected page_image".into())) };
        let model = model_param(params)?;
        let spec = self.client.spec(model).map_err(model_err)?;
        let parser = parser_for(&spec.family).map_err(|e| StepError::InvalidParams(e.to_string()))?;
        let language = ctx.profile.language.as_str();
        let prompt = resolve_prompt(params, &spec.family, "layout", &prompts::LAYOUT_DETECT, language);
        let json_schema = (spec.family == "generic_json").then(|| blocks_schema(false));
        tracing::debug!(step = self.kind(), model, node = %ctx.node_id, "model_step");
        let resp = self
            .client
            .chat(model, &ChatRequest {
                prompt: prompt.text,
                images: vec![(page.bytes.clone(), "image/png")],
                max_tokens: max_tokens(params, 4096),
                json_schema,
                ..Default::default()
            })
            .await
            .map_err(model_err)?;
        let blocks = parser
            .parse(&resp.text, page, (page.width_px, page.height_px), &ctx.node_id)
            .map_err(|e| parse_err(&spec.family, self.kind(), e))?;
        let regions = regions_from_blocks(&blocks);
        tracing::debug!(step = self.kind(), regions = regions.len(), "layout detected");
        Ok(StepOutput { artifact: Artifact::Layout(regions), model_ref: Some(spec.model_ref()), prompt_ref: Some(prompt.reference) })
    }
}

// ---------------------------------------------------------------------------------------
// region.ocr / table.recognize / formula.recognize
// ---------------------------------------------------------------------------------------

/// `page_image, layout → blocks` for region-level recognition (text/handwriting, tables, formulas).
pub struct RegionStep {
    pub client: Arc<OpenAiClient>,
    kind: &'static str,
    task: &'static str,
    target: &'static [BlockKind],
}

impl RegionStep {
    pub fn ocr(client: Arc<OpenAiClient>) -> Self {
        Self {
            client,
            kind: "region.ocr",
            task: "ocr",
            target: &[BlockKind::Title, BlockKind::Heading, BlockKind::Paragraph, BlockKind::List, BlockKind::Caption,
                      BlockKind::Footnote, BlockKind::Handwriting, BlockKind::Reference, BlockKind::Stamp],
        }
    }
    pub fn table(client: Arc<OpenAiClient>) -> Self {
        Self { client, kind: "table.recognize", task: "table", target: &[BlockKind::Table] }
    }
    pub fn formula(client: Arc<OpenAiClient>) -> Self {
        Self { client, kind: "formula.recognize", task: "formula", target: &[BlockKind::Formula] }
    }
    pub fn targets(&self) -> &'static [BlockKind] {
        self.target
    }

    fn generic_prompt(&self) -> &'static Prompt {
        match self.task {
            "table" => &prompts::TABLE_RECOGNIZE,
            "formula" => &prompts::FORMULA_RECOGNIZE,
            _ => &prompts::REGION_OCR,
        }
    }

    fn default_max_tokens(&self) -> u32 {
        match self.task {
            "table" => 4096,
            "formula" => 1024,
            _ => 2048,
        }
    }
}

/// Turns one region's raw model output into block content. Structured family output (JSON /
/// tagged) is parsed and reduced to the region's kind; anything else is taken as plain
/// text / HTML / LaTeX.
pub fn region_content(parser: Option<&dyn OutputParser>, kind: BlockKind, raw: &str, crop_size: (u32, u32)) -> (BlockContent, String) {
    let structured = raw.contains("<|box_start|>") || extract_json(raw).is_some_and(|v| v.is_array() || v.is_object());
    if let (true, Some(p)) = (structured, parser) {
        let dummy = PageImage { uri: String::new(), bytes: Arc::from(Vec::new()), width_px: crop_size.0, height_px: crop_size.1, dpi: 0 };
        if let Ok(blocks) = p.parse(raw, &dummy, crop_size, "") {
            if !blocks.is_empty() {
                let reduced = match kind {
                    BlockKind::Table => blocks
                        .iter()
                        .find_map(|b| match &b.content {
                            BlockContent::Table { html } => Some(html.clone()),
                            _ => None,
                        })
                        .unwrap_or_else(|| blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")),
                    BlockKind::Formula => blocks
                        .iter()
                        .find_map(|b| match &b.content {
                            BlockContent::Formula { latex } => Some(latex.clone()),
                            _ => None,
                        })
                        .unwrap_or_else(|| blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")),
                    _ => blocks.iter().map(|b| b.text.as_str()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n"),
                };
                return content_for(kind, &reduced);
            }
        }
    }
    content_for(kind, raw)
}

#[async_trait]
impl Step for RegionStep {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::PageImage, ArtifactKind::Layout], output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        120_000
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &Value) -> Result<StepOutput, StepError> {
        let (Artifact::PageImage(page), Artifact::Layout(layout)) = (inputs[0], inputs[1]) else {
            return Err(StepError::InvalidParams("expected page_image, layout".into()));
        };
        let model = model_param(params)?;
        let spec = self.client.spec(model).map_err(model_err)?;
        let language = ctx.profile.language.as_str();
        let prompt = resolve_prompt(params, &spec.family, self.task, self.generic_prompt(), language);
        let wanted: Vec<BlockKind> = kinds_param(params)?.unwrap_or_else(|| self.target.to_vec());
        let padding = params["crop_padding_px"].as_u64().map(|v| v as u32).unwrap_or(CROP_PADDING_PX);
        let selected: Vec<LayoutRegion> = layout.iter().filter(|r| wanted.contains(&r.kind)).cloned().collect();
        tracing::debug!(step = self.kind(), model, node = %ctx.node_id, regions = selected.len(), "model_step");
        if selected.is_empty() {
            return Ok(StepOutput { artifact: Artifact::Blocks(vec![]), model_ref: Some(spec.model_ref()), prompt_ref: Some(prompt.reference) });
        }
        let crops = crop_regions(page, selected, padding).await?;
        let parser = parser_or_none(&spec.family);
        let max_tokens = max_tokens(params, self.default_max_tokens());
        let client = &self.client;
        let prompt_text = &prompt.text;
        let results = run_bounded(crops, spec.max_concurrency, |crop| async move {
            let req = ChatRequest {
                prompt: prompt_text.clone(),
                images: vec![(crop.png.clone(), "image/png")],
                max_tokens,
                ..Default::default()
            };
            let res = client.chat(model, &req).await;
            (crop, res)
        })
        .await;
        let mut blocks = Vec::with_capacity(results.len());
        for (crop, res) in results {
            let resp = res.map_err(model_err)?;
            let (content, text) = region_content(parser.as_deref(), crop.region.kind, &resp.text, (crop.width_px, crop.height_px));
            blocks.push(BlockDraft {
                kind: crop.region.kind,
                bbox: crop.region.bbox,
                reading_order: crop.region.reading_order,
                content,
                text,
                level: None,
                confidence: crop.region.score.clamp(0.0, 1.0),
                node_id: ctx.node_id.clone(),
            });
        }
        blocks.sort_by_key(|b| b.reading_order);
        tracing::debug!(step = self.kind(), blocks = blocks.len(), "regions recognised");
        Ok(StepOutput { artifact: Artifact::Blocks(blocks), model_ref: Some(spec.model_ref()), prompt_ref: Some(prompt.reference) })
    }
}

// ---------------------------------------------------------------------------------------
// figure.describe
// ---------------------------------------------------------------------------------------

/// `figure_crops → blocks`
pub struct FigureDescribe {
    pub client: Arc<OpenAiClient>,
}

#[async_trait]
impl Step for FigureDescribe {
    fn kind(&self) -> &'static str {
        "figure.describe"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::FigureCrops], output: ArtifactKind::Blocks }
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &Value) -> Result<StepOutput, StepError> {
        let Artifact::FigureCrops(crops) = inputs[0] else { return Err(StepError::InvalidParams("expected figure_crops".into())) };
        let model = model_param(params)?;
        let spec = self.client.spec(model).map_err(model_err)?;
        let prompt = resolve_prompt(params, "", "figure_describe", &prompts::FIGURE_DESCRIBE, ctx.profile.language.as_str());
        let schema = json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "figure_type": {"type": "string"},
                "text_in_image": {"type": "string"},
                "entities": {"type": "array", "items": {"type": "object",
                    "properties": {"label": {"type": "string"}, "type": {"type": "string"}}, "required": ["label", "type"]}},
                "relations": {"type": "array", "items": {"type": "object",
                    "properties": {"subject": {"type": "string"}, "predicate": {"type": "string"}, "object": {"type": "string"}},
                    "required": ["subject", "predicate", "object"]}}
            },
            "required": ["description", "figure_type", "text_in_image", "entities", "relations"]
        });
        tracing::debug!(step = self.kind(), model, node = %ctx.node_id, crops = crops.len(), "model_step");
        let max_tokens = max_tokens(params, 1024);
        let client = &self.client;
        let (prompt_text, schema) = (&prompt.text, &schema);
        let results = run_bounded(crops.iter().collect::<Vec<_>>(), spec.max_concurrency, |crop| async move {
            let req = ChatRequest {
                prompt: prompt_text.clone(),
                images: vec![(crop.bytes.clone(), "image/png")],
                max_tokens,
                json_schema: Some(schema.clone()),
                ..Default::default()
            };
            (crop, client.chat(model, &req).await)
        })
        .await;
        let mut blocks = Vec::with_capacity(crops.len());
        for (crop, res) in results {
            let resp = res.map_err(model_err)?;
            let v: Value = extract_json(&resp.text).ok_or_else(|| parse_err("generic_json", self.kind(), "figure description is not JSON"))?;
            let description = v["description"].as_str().unwrap_or_default().to_string();
            // Stage 2 (vs-extract `visual.rs`) reads entities/relations from the description when
            // it is the step's JSON; a plain description is kept when the model found none.
            let has_structure = |k: &str| v[k].as_array().is_some_and(|a| !a.is_empty());
            let stored = if has_structure("entities") || has_structure("relations") { v.to_string() } else { description.clone() };
            blocks.push(BlockDraft {
                kind: crop.region.kind,
                bbox: crop.region.bbox,
                reading_order: crop.region.reading_order,
                content: BlockContent::Figure { crop_uri: crop.uri.clone(), description: Some(stored) },
                // Description + in-image text form the searchable text.
                text: format!("{}\n{}", description, v["text_in_image"].as_str().unwrap_or_default()).trim().to_string(),
                level: None,
                confidence: 0.8,
                node_id: ctx.node_id.clone(),
            });
        }
        blocks.sort_by_key(|b| b.reading_order);
        Ok(StepOutput { artifact: Artifact::Blocks(blocks), model_ref: Some(spec.model_ref()), prompt_ref: Some(prompt.reference) })
    }
}

// ---------------------------------------------------------------------------------------
// chart.to_table
// ---------------------------------------------------------------------------------------

/// `figure_crops → blocks`: chart crops → data table HTML + short description. Only crops whose
/// region kind is `chart` are processed (`params.kinds` may widen this, e.g. `["chart","figure"]`).
pub struct ChartToTable {
    pub client: Arc<OpenAiClient>,
}

#[async_trait]
impl Step for ChartToTable {
    fn kind(&self) -> &'static str {
        "chart.to_table"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::FigureCrops], output: ArtifactKind::Blocks }
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &Value) -> Result<StepOutput, StepError> {
        let Artifact::FigureCrops(crops) = inputs[0] else { return Err(StepError::InvalidParams("expected figure_crops".into())) };
        let model = model_param(params)?;
        let spec = self.client.spec(model).map_err(model_err)?;
        let prompt = resolve_prompt(params, "", "chart", &prompts::CHART_TO_TABLE, ctx.profile.language.as_str());
        let wanted = kinds_param(params)?.unwrap_or_else(|| vec![BlockKind::Chart]);
        let selected: Vec<_> = crops.iter().filter(|c| wanted.contains(&c.region.kind)).collect();
        let schema = json!({
            "type": "object",
            "properties": {
                "chart_type": {"type": "string"},
                "title": {"type": "string"},
                "x_axis": {"type": "string"},
                "y_axis": {"type": "string"},
                "table_html": {"type": "string"},
                "description": {"type": "string"}
            },
            "required": ["chart_type", "title", "x_axis", "y_axis", "table_html", "description"]
        });
        tracing::debug!(step = self.kind(), model, node = %ctx.node_id, crops = selected.len(), "model_step");
        let max_tokens = max_tokens(params, 2048);
        let client = &self.client;
        let (prompt_text, schema) = (&prompt.text, &schema);
        let results = run_bounded(selected, spec.max_concurrency, |crop| async move {
            let req = ChatRequest {
                prompt: prompt_text.clone(),
                images: vec![(crop.bytes.clone(), "image/png")],
                max_tokens,
                json_schema: Some(schema.clone()),
                ..Default::default()
            };
            (crop, client.chat(model, &req).await)
        })
        .await;
        let mut blocks = Vec::with_capacity(results.len());
        for (crop, res) in results {
            let resp = res.map_err(model_err)?;
            let v: Value = extract_json(&resp.text).ok_or_else(|| parse_err("generic_json", self.kind(), "chart output is not JSON"))?;
            let description = v["description"].as_str().unwrap_or_default().trim().to_string();
            let (table_content, table_text) = content_for(BlockKind::Table, v["table_html"].as_str().unwrap_or_default());
            let data_html = match table_content {
                BlockContent::Table { html } if !html.is_empty() => Some(html),
                _ => None,
            };
            let title = v["title"].as_str().unwrap_or_default().trim();
            let mut text_parts: Vec<&str> = Vec::new();
            if !title.is_empty() {
                text_parts.push(title);
            }
            if !description.is_empty() {
                text_parts.push(&description);
            }
            if !table_text.is_empty() {
                text_parts.push(&table_text);
            }
            blocks.push(BlockDraft {
                kind: BlockKind::Chart,
                bbox: crop.region.bbox,
                reading_order: crop.region.reading_order,
                content: BlockContent::Chart {
                    crop_uri: crop.uri.clone(),
                    data_html,
                    description: if description.is_empty() { None } else { Some(description.clone()) },
                },
                text: text_parts.join("\n"),
                level: None,
                confidence: 0.7,
                node_id: ctx.node_id.clone(),
            });
        }
        blocks.sort_by_key(|b| b.reading_order);
        Ok(StepOutput { artifact: Artifact::Blocks(blocks), model_ref: Some(spec.model_ref()), prompt_ref: Some(prompt.reference) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_param_parses_and_rejects() {
        assert_eq!(kinds_param(&json!({})).unwrap(), None);
        assert_eq!(kinds_param(&json!({"kinds": ["table", "Section-header"]})).unwrap(), Some(vec![BlockKind::Table, BlockKind::Heading]));
        assert!(kinds_param(&json!({"kinds": ["nope"]})).is_err());
        assert!(kinds_param(&json!({"kinds": "table"})).is_err());
    }

    #[test]
    fn prompt_resolution_order() {
        let r = resolve_prompt(&json!({}), "paddleocr_vl", "table", &prompts::TABLE_RECOGNIZE, "en");
        assert_eq!((r.text.as_str(), r.reference.starts_with("paddleocr_vl_table@")), ("Table Recognition:", true));
        let r = resolve_prompt(&json!({}), "generic_json", "table", &prompts::TABLE_RECOGNIZE, "de");
        assert!(r.text.contains("language is de") && r.reference.starts_with("table_recognize@"));
        let r = resolve_prompt(&json!({"prompt": "region_ocr"}), "paddleocr_vl", "table", &prompts::TABLE_RECOGNIZE, "en");
        assert!(r.reference.starts_with("region_ocr@"));
        let r = resolve_prompt(&json!({"prompt": "Read it in {language}"}), "x", "y", &prompts::REGION_OCR, "fr");
        assert_eq!((r.text.as_str(), r.reference.starts_with("inline@")), ("Read it in fr", true));
    }

    #[test]
    fn crop_stays_inside_image() {
        let img = DynamicImage::new_rgb8(100, 50);
        let (png, w, h) = crop_png(&img, &BBox::new(0.9, 0.8, 1.0, 1.0).unwrap(), 4).unwrap();
        assert_eq!((w, h), (14, 14));
        assert!(image::load_from_memory(&png).is_ok());
        let (_, w, h) = crop_png(&img, &BBox::new(0.5, 0.5, 0.5, 0.5).unwrap(), 0).unwrap();
        assert_eq!((w, h), (1, 1));
    }

    #[test]
    fn region_content_reduces_structured_output() {
        let p = parser_for("dots_ocr").unwrap();
        let raw = r#"[{"bbox":[0,0,10,10],"category":"Table","text":"<table><tr><td>1</td></tr></table>"}]"#;
        let (c, t) = region_content(Some(p.as_ref()), BlockKind::Table, raw, (10, 10));
        assert_eq!(c, BlockContent::Table { html: "<table><tr><td>1</td></tr></table>".into() });
        assert_eq!(t, "1");
        let (c, _) = region_content(Some(p.as_ref()), BlockKind::Paragraph, "plain words", (10, 10));
        assert_eq!(c, BlockContent::Text { text: "plain words".into() });
        let (c, _) = region_content(None, BlockKind::Formula, "```latex\n$$x^2$$\n```", (10, 10));
        assert_eq!(c, BlockContent::Formula { latex: "x^2".into() });
    }

    #[test]
    fn schema_shapes() {
        let s = blocks_schema(false);
        assert_eq!(s["required"][0], "regions");
        let s = blocks_schema(true);
        assert!(s["properties"]["blocks"]["items"]["properties"]["text"].is_object());
    }
}
