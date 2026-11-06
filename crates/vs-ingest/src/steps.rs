//! CPU-only DAG steps.

use async_trait::async_trait;

use vs_core::{BBox, BlockContent, BlockKind};
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
///
/// Params: `{"padding": 0.02}` — normalised margin added on every side of a
/// `figure`/`chart` region before cropping (clamped to the page).
///
/// `StepContext` carries no blob store, so crops stay in memory; `FigureCrop::uri` is a
/// `crop://<page_hk>/<node_id>/<n>.png` placeholder the uploading side can replace.
pub struct FigureCrop;

/// Default normalised padding around a figure region (2 % of the page per side).
pub const DEFAULT_CROP_PADDING: f32 = 0.02;

#[derive(serde::Deserialize, Default)]
struct FigureCropParams {
    padding: Option<f32>,
}

#[async_trait]
impl Step for FigureCrop {
    fn kind(&self) -> &'static str {
        "figure.crop"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::PageImage, ArtifactKind::Layout], output: ArtifactKind::FigureCrops }
    }
    fn default_timeout_ms(&self) -> u64 {
        20_000
    }
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let (Artifact::PageImage(page), Artifact::Layout(layout)) = (inputs[0], inputs[1]) else {
            return Err(StepError::InvalidParams("unexpected input kinds".into()));
        };
        let p: FigureCropParams = if params.is_null() {
            FigureCropParams::default()
        } else {
            serde_json::from_value(params.clone()).map_err(|e| StepError::InvalidParams(e.to_string()))?
        };
        let padding = p.padding.unwrap_or(DEFAULT_CROP_PADDING);
        if !(0.0..=0.5).contains(&padding) || padding.is_nan() {
            return Err(StepError::InvalidParams(format!("padding must be within 0..=0.5, got {padding}")));
        }
        let uri_prefix = format!("crop://{}/{}", ctx.page_hk, ctx.node_id);
        // Decoding and re-encoding a 200 dpi page is CPU work; keep it off the async executor.
        let bytes = page.bytes.clone();
        let regions = layout.clone();
        let crops = tokio::task::spawn_blocking(move || crop_figures(&bytes, &regions, padding, &uri_prefix))
            .await
            .map_err(|e| StepError::Io(format!("figure.crop task failed: {e}")))??;
        Ok(StepOutput { artifact: Artifact::FigureCrops(crops), model_ref: None, prompt_ref: None })
    }
}

/// Region kinds that `figure.crop` cuts out.
pub fn is_figure_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Figure | BlockKind::Chart)
}

/// Expands `bbox` by `padding` on every side, clamped to the page.
pub fn pad_bbox(bbox: &BBox, padding: f32) -> BBox {
    BBox {
        x0: (bbox.x0 - padding).clamp(0.0, 1.0),
        y0: (bbox.y0 - padding).clamp(0.0, 1.0),
        x1: (bbox.x1 + padding).clamp(0.0, 1.0),
        y1: (bbox.y1 + padding).clamp(0.0, 1.0),
    }
}

/// Decodes the page image and crops every figure/chart region, padded by `padding`.
///
/// The returned `FigureCrop::region` keeps the source kind, score and reading order but
/// carries the **padded** bbox, i.e. the exact area of the page the crop bytes show.
/// Regions that collapse to zero pixels are skipped. Crops are PNG-encoded.
pub fn crop_figures(page_bytes: &[u8], layout: &[LayoutRegion], padding: f32, uri_prefix: &str) -> Result<Vec<vs_dag::FigureCrop>, StepError> {
    let mut regions: Vec<&LayoutRegion> = layout.iter().filter(|r| is_figure_kind(r.kind)).collect();
    if regions.is_empty() {
        return Ok(vec![]);
    }
    regions.sort_by_key(|r| r.reading_order);
    let img = image::load_from_memory(page_bytes).map_err(|e| StepError::Io(format!("decode page image: {e}")))?;
    let (w, h) = (img.width(), img.height());
    let mut out = Vec::with_capacity(regions.len());
    for (n, region) in regions.into_iter().enumerate() {
        let padded = pad_bbox(&region.bbox, padding);
        let px = |v: f32, dim: u32| (v * dim as f32).round().clamp(0.0, dim as f32) as u32;
        let (x0, y0, x1, y1) = (px(padded.x0, w), px(padded.y0, h), px(padded.x1, w), px(padded.y1, h));
        if x1 <= x0 || y1 <= y0 {
            tracing::debug!(reading_order = region.reading_order, "figure.crop: skipping empty region");
            continue;
        }
        let crop = img.crop_imm(x0, y0, x1 - x0, y1 - y0);
        let mut png = std::io::Cursor::new(Vec::new());
        crop.write_to(&mut png, image::ImageFormat::Png).map_err(|e| StepError::Io(format!("encode crop: {e}")))?;
        out.push(vs_dag::FigureCrop {
            region: LayoutRegion { kind: region.kind, bbox: padded, reading_order: region.reading_order, score: region.score },
            uri: format!("{uri_prefix}/{n}.png"),
            bytes: std::sync::Arc::from(png.into_inner().into_boxed_slice()),
        });
    }
    Ok(out)
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

    fn synthetic_page() -> (Vec<u8>, u32, u32) {
        // 200x100 white page with a 40x20 red rectangle at (100,40)-(140,60).
        let mut img = image::RgbImage::from_pixel(200, 100, image::Rgb([255, 255, 255]));
        for y in 40..60 {
            for x in 100..140 {
                img.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        (buf.into_inner(), 200, 100)
    }

    #[test]
    fn crops_figure_regions_with_padding() {
        let (png, _, _) = synthetic_page();
        let layout = vec![
            LayoutRegion { kind: BlockKind::Paragraph, bbox: BBox::new(0.0, 0.0, 1.0, 0.2).unwrap(), reading_order: 0, score: 0.9 },
            LayoutRegion { kind: BlockKind::Figure, bbox: BBox::new(0.5, 0.4, 0.7, 0.6).unwrap(), reading_order: 1, score: 0.8 },
        ];
        // padding 0.05 → x 0.45..0.75 (90..150 px), y 0.35..0.65 (35..65 px)
        let crops = crop_figures(&png, &layout, 0.05, "crop://p/n").unwrap();
        assert_eq!(crops.len(), 1);
        let c = &crops[0];
        assert_eq!(c.region.kind, BlockKind::Figure);
        assert_eq!(c.region.reading_order, 1);
        assert_eq!(c.uri, "crop://p/n/0.png");
        assert!((c.region.bbox.x0 - 0.45).abs() < 1e-6 && (c.region.bbox.y1 - 0.65).abs() < 1e-6);
        let img = image::load_from_memory(&c.bytes).unwrap().to_rgb8();
        assert_eq!((img.width(), img.height()), (60, 30));
        // red block sits 10 px inside the padded crop on each side
        assert_eq!(img.get_pixel(0, 0), &image::Rgb([255, 255, 255]));
        assert_eq!(img.get_pixel(10, 10), &image::Rgb([255, 0, 0]));
        assert_eq!(img.get_pixel(49, 24), &image::Rgb([255, 0, 0]));
        assert_eq!(img.get_pixel(50, 25), &image::Rgb([255, 255, 255]));
    }

    #[test]
    fn padding_is_clamped_to_the_page_and_zero_regions_skipped() {
        let (png, _, _) = synthetic_page();
        let layout = vec![
            LayoutRegion { kind: BlockKind::Chart, bbox: BBox::new(0.9, 0.9, 1.0, 1.0).unwrap(), reading_order: 3, score: 0.8 },
            LayoutRegion { kind: BlockKind::Figure, bbox: BBox::new(0.5, 0.5, 0.5, 0.5).unwrap(), reading_order: 2, score: 0.8 },
        ];
        let crops = crop_figures(&png, &layout, 0.0, "crop://p/n").unwrap();
        assert_eq!(crops.len(), 1, "zero-area region is skipped");
        let crops = crop_figures(&png, &layout, 0.2, "crop://p/n").unwrap();
        assert_eq!(crops.len(), 2, "padding makes the point region croppable");
        assert_eq!(crops[0].region.reading_order, 2, "crops follow reading order");
        let c = &crops[1];
        assert_eq!(c.region.kind, BlockKind::Chart);
        assert_eq!((c.region.bbox.x1, c.region.bbox.y1), (1.0, 1.0));
        let img = image::load_from_memory(&c.bytes).unwrap();
        assert_eq!((img.width(), img.height()), (60, 30));
    }

    #[tokio::test]
    async fn step_reads_padding_param_and_rejects_bad_values() {
        use std::sync::Arc;
        use vs_core::{ClassifierTier, DocType, HashKey, LanguageTag, PageProfile, RunId};
        let (png, w, h) = synthetic_page();
        let page = Artifact::PageImage(vs_dag::PageImage { uri: "mem".into(), bytes: Arc::from(png.into_boxed_slice()), width_px: w, height_px: h, dpi: 72 });
        let layout = Artifact::Layout(vec![LayoutRegion { kind: BlockKind::Figure, bbox: BBox::new(0.5, 0.4, 0.7, 0.6).unwrap(), reading_order: 0, score: 0.8 }]);
        let ctx = StepContext {
            run_id: RunId::new(),
            document_hk: HashKey::of(&["d"]),
            page_hk: HashKey::of(&["p"]),
            page_no: 1,
            profile: PageProfile {
                language: LanguageTag::parse("en").unwrap(),
                doc_type: DocType::Other,
                born_digital: true,
                text_layer_coverage: 1.0,
                has_handwriting: false,
                has_images: true,
                has_tables: false,
                has_formulas: false,
                has_charts: false,
                rotation_deg: 0,
                confidence: 1.0,
                tier: ClassifierTier::Rules,
            },
            dag_ref: "t@1".into(),
            node_id: "crop".into(),
        };
        let out = FigureCrop.run(&ctx, &[&page, &layout], &serde_json::json!({"padding": 0.0})).await.unwrap();
        let Artifact::FigureCrops(crops) = out.artifact else { panic!("wrong artifact kind") };
        assert_eq!(crops.len(), 1);
        let img = image::load_from_memory(&crops[0].bytes).unwrap();
        assert_eq!((img.width(), img.height()), (40, 20));
        assert!(crops[0].uri.starts_with(&format!("crop://{}/crop/", ctx.page_hk)));

        let err = FigureCrop.run(&ctx, &[&page, &layout], &serde_json::json!({"padding": 0.9})).await.unwrap_err();
        assert!(matches!(err, StepError::InvalidParams(_)));
        // null params → default padding
        assert!(FigureCrop.run(&ctx, &[&page, &layout], &serde_json::Value::Null).await.is_ok());
    }
}
