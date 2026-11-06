//! CPU-only DAG steps.

use async_trait::async_trait;

use vs_core::{BlockContent, BlockKind};
use vs_dag::{Artifact, ArtifactKind, BlockDraft, LayoutRegion, Signature, Step, StepContext, StepError, StepOutput, TextLayer};

/// `text_layer, layout, blocks → blocks`
pub struct MergeNative;

#[async_trait]
impl Step for MergeNative {
    fn kind(&self) -> &'static str {
        "blocks.merge_native"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::TextLayer, ArtifactKind::Layout, ArtifactKind::Blocks], output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        5_000
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], _params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let (Artifact::TextLayer(text), Artifact::Layout(layout), Artifact::Blocks(recognised)) = (inputs[0], inputs[1], inputs[2]) else {
            return Err(StepError::InvalidParams("unexpected input kinds".into()));
        };
        Ok(StepOutput {
            artifact: Artifact::Blocks(merge_native(text, layout, recognised, &ctx.node_id)),
            model_ref: None,
            prompt_ref: None,
        })
    }
}

/// Assigns native text spans to layout regions; recognised blocks win where they overlap a region.
pub fn merge_native(text: &TextLayer, layout: &[LayoutRegion], recognised: &[BlockDraft], node_id: &str) -> Vec<BlockDraft> {
    let mut out: Vec<BlockDraft> = recognised.to_vec();
    let covered = |r: &LayoutRegion| recognised.iter().any(|b| b.bbox.intersection_over_union(&r.bbox) >= 0.5);
    let mut used = vec![false; text.spans.len()];

    for region in layout.iter().filter(|r| !covered(r)) {
        let mut parts = Vec::new();
        for (i, span) in text.spans.iter().enumerate() {
            let cx = (span.bbox.x0 + span.bbox.x1) / 2.0;
            let cy = (span.bbox.y0 + span.bbox.y1) / 2.0;
            if !used[i] && cx >= region.bbox.x0 && cx <= region.bbox.x1 && cy >= region.bbox.y0 && cy <= region.bbox.y1 {
                used[i] = true;
                parts.push(span.text.as_str());
            }
        }
        let joined = parts.join(" ");
        let content = if joined.is_empty() { BlockContent::Empty } else { BlockContent::Text { text: joined.clone() } };
        out.push(BlockDraft {
            kind: region.kind,
            bbox: region.bbox,
            reading_order: region.reading_order,
            content,
            text: joined,
            level: None,
            confidence: 0.99,
            node_id: node_id.to_string(),
        });
    }

    let max_order = out.iter().map(|b| b.reading_order).max().unwrap_or(0);
    for (i, span) in text.spans.iter().enumerate().filter(|(i, _)| !used[*i]) {
        out.push(BlockDraft {
            kind: BlockKind::Other,
            bbox: span.bbox,
            reading_order: max_order + 1 + i as u32,
            content: BlockContent::Text { text: span.text.clone() },
            text: span.text.clone(),
            level: None,
            confidence: 0.9,
            node_id: node_id.to_string(),
        });
    }
    out.sort_by_key(|b| b.reading_order);
    out
}

/// `page_image, layout → figure_crops`
pub struct FigureCrop;

#[async_trait]
impl Step for FigureCrop {
    fn kind(&self) -> &'static str {
        "figure.crop"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::PageImage, ArtifactKind::Layout], output: ArtifactKind::FigureCrops }
    }
    async fn run(&self, _ctx: &StepContext, _inputs: &[&Artifact], _params: &serde_json::Value) -> Result<StepOutput, StepError> {
        Err(StepError::NotImplemented("figure.crop: decode page, crop figure/chart regions with padding, upload"))
    }
}

/// `blocks, blocks → blocks`
pub struct MergeBlocks;

#[async_trait]
impl Step for MergeBlocks {
    fn kind(&self) -> &'static str {
        "blocks.merge"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::Blocks, ArtifactKind::Blocks], output: ArtifactKind::Blocks }
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], _params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let (Artifact::Blocks(a), Artifact::Blocks(b)) = (inputs[0], inputs[1]) else {
            return Err(StepError::InvalidParams("unexpected input kinds".into()));
        };
        let mut out = a.clone();
        for nb in b {
            match out.iter_mut().find(|x| x.bbox.intersection_over_union(&nb.bbox) >= 0.5) {
                Some(existing) if nb.confidence > existing.confidence => *existing = nb.clone(),
                Some(_) => {}
                None => out.push(nb.clone()),
            }
        }
        out.sort_by_key(|b| b.reading_order);
        Ok(StepOutput { artifact: Artifact::Blocks(out), model_ref: None, prompt_ref: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::BBox;
    use vs_dag::TextSpan;

    #[test]
    fn spans_go_to_containing_region() {
        let text = TextLayer {
            spans: vec![
                TextSpan { text: "Hello".into(), bbox: BBox::new(0.1, 0.1, 0.2, 0.12).unwrap(), font_size: None },
                TextSpan { text: "world".into(), bbox: BBox::new(0.21, 0.1, 0.3, 0.12).unwrap(), font_size: None },
                TextSpan { text: "stray".into(), bbox: BBox::new(0.8, 0.9, 0.9, 0.92).unwrap(), font_size: None },
            ],
        };
        let layout = vec![LayoutRegion { kind: BlockKind::Paragraph, bbox: BBox::new(0.05, 0.05, 0.5, 0.2).unwrap(), reading_order: 0, score: 0.9 }];
        let out = merge_native(&text, &layout, &[], "merge");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "Hello world");
        assert_eq!(out[1].kind, BlockKind::Other);
    }
}
