//! Model-backed DAG steps. See SPEC.md.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use vs_core::{BlockContent, BlockKind};
use vs_dag::{Artifact, ArtifactKind, BlockDraft, Signature, Step, StepContext, StepError, StepOutput};

use crate::client::{ChatRequest, OpenAiClient};
use crate::parsers::parser_for;
use crate::prompts;

fn model_param(params: &Value) -> Result<&str, StepError> {
    params["model"].as_str().ok_or_else(|| StepError::InvalidParams("`model` is required".into()))
}

fn max_tokens(params: &Value, default: u32) -> u32 {
    params["max_tokens"].as_u64().map(|v| v as u32).unwrap_or(default)
}

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
        let spec = self.client.spec(model).map_err(|e| StepError::Model(e.to_string()))?;
        let parser = parser_for(&spec.family).map_err(|e| StepError::Parse(e.to_string()))?;
        // Family-specific task prompt (e.g. PaddleOCR-VL "OCR:"-style instructions) comes from params.
        let prompt = params["task_prompt"].as_str().unwrap_or("Parse this document page.").to_string();
        let resp = self
            .client
            .chat(model, &ChatRequest {
                prompt,
                images: vec![(page.bytes.clone(), "image/png")],
                max_tokens: max_tokens(params, 8192),
                ..Default::default()
            })
            .await
            .map_err(|e| StepError::Model(e.to_string()))?;
        let blocks = parser
            .parse(&resp.text, page, (page.width_px, page.height_px), &ctx.node_id)
            .map_err(|e| StepError::Parse(e.to_string()))?;
        Ok(StepOutput { artifact: Artifact::Blocks(blocks), model_ref: Some(spec.model_ref()), prompt_ref: None })
    }
}

/// `page_image → layout`
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
    async fn run(&self, _ctx: &StepContext, _inputs: &[&Artifact], _params: &Value) -> Result<StepOutput, StepError> {
        Err(StepError::NotImplemented("layout.detect: call layout model, parse regions + reading order"))
    }
}

/// `page_image, layout → blocks` for region-level recognition (text/handwriting, tables, formulas).
pub struct RegionStep {
    pub client: Arc<OpenAiClient>,
    kind: &'static str,
    target: &'static [BlockKind],
}

impl RegionStep {
    pub fn ocr(client: Arc<OpenAiClient>) -> Self {
        Self {
            client,
            kind: "region.ocr",
            target: &[BlockKind::Title, BlockKind::Heading, BlockKind::Paragraph, BlockKind::List, BlockKind::Caption,
                      BlockKind::Footnote, BlockKind::Handwriting, BlockKind::Reference, BlockKind::Stamp],
        }
    }
    pub fn table(client: Arc<OpenAiClient>) -> Self {
        Self { client, kind: "table.recognize", target: &[BlockKind::Table] }
    }
    pub fn formula(client: Arc<OpenAiClient>) -> Self {
        Self { client, kind: "formula.recognize", target: &[BlockKind::Formula] }
    }
    pub fn targets(&self) -> &'static [BlockKind] {
        self.target
    }
}

#[async_trait]
impl Step for RegionStep {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::PageImage, ArtifactKind::Layout], output: ArtifactKind::Blocks }
    }
    async fn run(&self, _ctx: &StepContext, _inputs: &[&Artifact], _params: &Value) -> Result<StepOutput, StepError> {
        Err(StepError::NotImplemented("region steps: crop target regions, one request each (concurrent), parse per family"))
    }
}

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
        let spec = self.client.spec(model).map_err(|e| StepError::Model(e.to_string()))?;
        let prompt = prompts::FIGURE_DESCRIBE.render(&[("language", ctx.profile.language.as_str())]);
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

        let futs = crops.iter().map(|crop| {
            let req = ChatRequest {
                prompt: prompt.clone(),
                images: vec![(crop.bytes.clone(), "image/png")],
                max_tokens: max_tokens(params, 1024),
                json_schema: Some(schema.clone()),
                ..Default::default()
            };
            async move { (crop, self.client.chat(model, &req).await) }
        });
        let mut blocks = Vec::with_capacity(crops.len());
        for (crop, res) in futures::future::join_all(futs).await {
            let resp = res.map_err(|e| StepError::Model(e.to_string()))?;
            let v: Value = serde_json::from_str(&resp.text).map_err(|e| StepError::Parse(e.to_string()))?;
            let description = v["description"].as_str().unwrap_or_default().to_string();
            blocks.push(BlockDraft {
                kind: crop.region.kind,
                bbox: crop.region.bbox,
                reading_order: crop.region.reading_order,
                content: BlockContent::Figure { crop_uri: crop.uri.clone(), description: Some(description.clone()) },
                // Description + in-image text form the searchable text; full JSON (entities/relations)
                // is kept by the worker in the page result for Stage 2.
                text: format!("{}\n{}", description, v["text_in_image"].as_str().unwrap_or_default()).trim().to_string(),
                level: None,
                confidence: 0.8,
                node_id: ctx.node_id.clone(),
            });
        }
        Ok(StepOutput {
            artifact: Artifact::Blocks(blocks),
            model_ref: Some(spec.model_ref()),
            prompt_ref: Some(prompts::FIGURE_DESCRIBE.reference()),
        })
    }
}

/// `figure_crops → blocks`
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
    async fn run(&self, _ctx: &StepContext, _inputs: &[&Artifact], _params: &Value) -> Result<StepOutput, StepError> {
        Err(StepError::NotImplemented("chart.to_table: like figure.describe with CHART_TO_TABLE prompt, only chart crops"))
    }
}
