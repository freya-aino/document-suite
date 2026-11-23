//! CPU-only DAG steps.

use async_trait::async_trait;

use vs_core::{BBox, BlockContent, BlockKind};
use vs_dag::{Artifact, ArtifactKind, BlockDraft, LayoutRegion, PageImage, Signature, Step, StepContext, StepError, StepOutput, TextLayer};

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
        Ok(StepOutput::plain(Artifact::Blocks(merge_native(text, layout, recognised, &ctx.node_id))))
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
    /// Cut the crops from a render of the page at this DPI instead of from the page
    /// image.
    ///
    /// A figure occupying a tenth of the page is otherwise described from a tenth of the
    /// page's pixels. The finer render comes from the original document through
    /// `StepContext::pages`; where that is unavailable — a deployment with
    /// `worker.max_crop_dpi = 0`, an original past its retention, an offline `vs-adapt`
    /// run over loose files — the page image is used and the fallback is logged with its
    /// reason, because "set it and nothing happened" is the failure this param had for
    /// three revisions.
    crop_dpi: Option<u32>,
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
        let source = finer_page(ctx, page, p.crop_dpi).await;
        // Decoding and re-encoding a 200 dpi page is CPU work; keep it off the async executor.
        let bytes = source.bytes.clone();
        let regions = layout.clone();
        let crops = tokio::task::spawn_blocking(move || crop_figures(&bytes, &regions, padding, &uri_prefix))
            .await
            .map_err(|e| StepError::Io(format!("figure.crop task failed: {e}")))??;
        Ok(StepOutput::plain(Artifact::FigureCrops(crops)))
    }
}

/// The page to cut from, honouring `crop_dpi`.
///
/// Returns the page image itself unless a finer render was asked for *and* can be had.
/// Each way of not getting one is a state rather than an error, and each is logged with
/// its reason.
async fn finer_page<'a>(ctx: &StepContext, page: &'a PageImage, crop_dpi: Option<u32>) -> std::borrow::Cow<'a, PageImage> {
    let Some(dpi) = crop_dpi.filter(|d| *d > 0) else { return std::borrow::Cow::Borrowed(page) };
    if dpi <= page.dpi {
        tracing::debug!(crop_dpi = dpi, page_dpi = page.dpi, "crop_dpi is not finer than the page image");
        return std::borrow::Cow::Borrowed(page);
    }
    let Some(pages) = ctx.pages.as_ref() else {
        tracing::warn!(crop_dpi = dpi, "crop_dpi was asked for but this deployment cannot re-render pages");
        return std::borrow::Cow::Borrowed(page);
    };
    match pages.page_at_dpi(ctx.page_no, dpi).await {
        Ok(finer) => {
            tracing::info!(node = %ctx.node_id, page_no = ctx.page_no, asked = dpi, got = finer.dpi, "cropping figures from a finer render");
            std::borrow::Cow::Owned(finer)
        }
        Err(e) => {
            tracing::warn!(crop_dpi = dpi, error = %e, "could not re-render the page; cropping from the page image");
            std::borrow::Cow::Borrowed(page)
        }
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
        Ok(StepOutput::plain(Artifact::Blocks(out)))
    }
}

/// `blocks, blocks → blocks`
pub struct VerifyBlocks;

/// How the two readings are compared and how much the second one is allowed to move a
/// confidence.
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
pub struct VerifyParams {
    /// Overlap at which a block in the second set counts as the *same* block. Matches
    /// `blocks.merge`, so a pair that merges is a pair that verifies.
    pub iou: f32,
    /// Share of the output confidence that comes from the second reading. 0 leaves every
    /// confidence alone; 1 discards the primary parser's own opinion.
    pub weight: f32,
    /// Multiplier applied to the agreement when the two readings overlap but disagree
    /// about the block *kind* — a paragraph one model read as a table is not nothing,
    /// but it is not corroboration either.
    pub kind_mismatch: f32,
}

impl Default for VerifyParams {
    fn default() -> Self {
        Self { iou: 0.5, weight: 0.5, kind_mismatch: 0.5 }
    }
}

#[async_trait]
impl Step for VerifyBlocks {
    fn kind(&self) -> &'static str {
        "blocks.verify"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::Blocks, ArtifactKind::Blocks], output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        5_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let (Artifact::Blocks(primary), Artifact::Blocks(second)) = (inputs[0], inputs[1]) else {
            return Err(StepError::InvalidParams("unexpected input kinds".into()));
        };
        let p: VerifyParams =
            if params.is_null() { VerifyParams::default() } else { serde_json::from_value(params.clone()).map_err(|e| StepError::InvalidParams(e.to_string()))? };
        for (name, v, hi) in [("iou", p.iou, 1.0), ("weight", p.weight, 1.0), ("kind_mismatch", p.kind_mismatch, 1.0)] {
            if !(0.0..=hi).contains(&v) || v.is_nan() {
                return Err(StepError::InvalidParams(format!("{name} must be within 0..={hi}, got {v}")));
            }
        }
        Ok(StepOutput::plain(Artifact::Blocks(verify_blocks(primary, second, &p))))
    }
}

/// Re-scores `primary` by how far a second reading of the same page corroborates it.
///
/// Membership is **not** changed: a block only the second parser saw is not added, and a
/// block it missed is not dropped. That is `blocks.merge`'s job, and keeping the two apart
/// is what makes the output interpretable — after a merge every block is present and you
/// cannot tell which parser vouched for it, whereas after a verify the confidence says so.
/// Running `blocks.merge` and then verifying the merged set against either input is the
/// combination that both unions and scores.
///
/// A block's new confidence is `(1 - weight)·own + weight·(agreement · witness)`, where
/// `witness` is the matched block's own confidence. Corroboration by a confident second
/// reading therefore pulls a middling block up, and a block no second reading saw at all
/// loses `weight` of its confidence outright — the one thing this step must do, because an
/// unwitnessed block is exactly what a downstream consumer wants flagged.
pub fn verify_blocks(primary: &[BlockDraft], second: &[BlockDraft], p: &VerifyParams) -> Vec<BlockDraft> {
    // Pair greedily from the best overlap down, one witness per block. A nearest-match
    // scan would let a single block in the second set vouch for two primary blocks, so a
    // spurious split would come out corroborated twice.
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (i, a) in primary.iter().enumerate() {
        for (j, b) in second.iter().enumerate() {
            let iou = a.bbox.intersection_over_union(&b.bbox);
            if iou >= p.iou {
                pairs.push((iou, i, j));
            }
        }
    }
    pairs.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
    let mut witness = vec![None; primary.len()];
    let mut taken = vec![false; second.len()];
    for (_, i, j) in pairs {
        if witness[i].is_none() && !taken[j] {
            witness[i] = Some(j);
            taken[j] = true;
        }
    }

    let mut out = primary.to_vec();
    for (block, w) in out.iter_mut().zip(witness) {
        let support = match w {
            None => 0.0,
            Some(j) => {
                let b = &second[j];
                let sim = text_agreement(&block.text, &b.text);
                let agreement = if block.kind == b.kind { sim } else { sim * p.kind_mismatch };
                agreement * b.confidence
            }
        };
        block.confidence = ((1.0 - p.weight) * block.confidence + p.weight * support).clamp(0.0, 1.0);
    }
    out
}

/// Token-overlap (Dice) agreement between two readings of the same block's text.
///
/// Token-based rather than character-based because the disagreements that matter here are
/// dropped or hallucinated words, not spelling: two parsers that read the same words in a
/// different order still agree about what is on the page, and an edit distance would not
/// say so. Two empty readings agree completely — both parsers found a block with no text,
/// which is a real and corroborated outcome for a figure or a rule.
pub fn text_agreement(a: &str, b: &str) -> f32 {
    let tokens = |s: &str| {
        let mut v: Vec<String> = s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase()).collect();
        v.sort();
        v
    };
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    // Multiset intersection over the two sorted token lists.
    let (mut i, mut j, mut shared) = (0, 0, 0usize);
    while i < ta.len() && j < tb.len() {
        match ta[i].cmp(&tb[j]) {
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    2.0 * shared as f32 / (ta.len() + tb.len()) as f32
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

    fn test_ctx(node_id: &str) -> StepContext {
        use vs_core::{ClassifierTier, DocType, HashKey, LanguageTag, PageProfile, RunId};
        StepContext {
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
            node_id: node_id.to_string(),
            interactive: false,
            pages: None,
        }
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
        let (png, w, h) = synthetic_page();
        let page = Artifact::PageImage(vs_dag::PageImage { uri: "mem".into(), bytes: Arc::from(png.into_boxed_slice()), width_px: w, height_px: h, dpi: 72 });
        let layout = Artifact::Layout(vec![LayoutRegion { kind: BlockKind::Figure, bbox: BBox::new(0.5, 0.4, 0.7, 0.6).unwrap(), reading_order: 0, score: 0.8 }]);
        let ctx = test_ctx("crop");
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

    fn draft(kind: BlockKind, x0: f32, text: &str, confidence: f32) -> BlockDraft {
        BlockDraft {
            kind,
            bbox: BBox::new(x0, 0.1, x0 + 0.2, 0.2).unwrap(),
            reading_order: 0,
            content: BlockContent::Text { text: text.into() },
            text: text.into(),
            level: None,
            confidence,
            node_id: "n".into(),
        }
    }

    #[test]
    fn agreement_is_over_tokens_not_characters() {
        assert_eq!(text_agreement("", ""), 1.0, "two parsers finding no text agree");
        assert_eq!(text_agreement("Total due", ""), 0.0);
        assert_eq!(text_agreement("Total due", "due Total"), 1.0, "word order is not disagreement");
        assert_eq!(text_agreement("Total due", "Total"), 2.0 / 3.0, "a dropped word is");
        // Repetition counts: the multiset, not the set.
        assert_eq!(text_agreement("a a", "a"), 2.0 / 3.0);
    }

    #[test]
    fn corroboration_lifts_and_absence_cuts() {
        let p = VerifyParams::default();
        let primary = vec![draft(BlockKind::Paragraph, 0.1, "Total due 42", 0.6), draft(BlockKind::Paragraph, 0.5, "only I saw this", 0.9)];
        let second = vec![draft(BlockKind::Paragraph, 0.1, "Total due 42", 1.0)];
        let out = verify_blocks(&primary, &second, &p);
        assert_eq!(out.len(), 2, "verify never changes membership");
        assert!((out[0].confidence - 0.8).abs() < 1e-6, "0.5*0.6 + 0.5*(1.0*1.0) = 0.8, got {}", out[0].confidence);
        assert!((out[1].confidence - 0.45).abs() < 1e-6, "unwitnessed: 0.5*0.9, got {}", out[1].confidence);
        // weight 0 is the identity, which is what makes the step safe to put in a DAG
        // before it has been tuned for a page class.
        let untouched = verify_blocks(&primary, &second, &VerifyParams { weight: 0.0, ..VerifyParams::default() });
        assert_eq!(untouched.iter().map(|b| b.confidence).collect::<Vec<_>>(), vec![0.6, 0.9]);
    }

    #[test]
    fn a_disagreement_about_kind_is_not_corroboration() {
        let p = VerifyParams::default();
        let primary = vec![draft(BlockKind::Paragraph, 0.1, "one two", 0.8)];
        let same_kind = verify_blocks(&primary, &[draft(BlockKind::Paragraph, 0.1, "one two", 0.8)], &p);
        let other_kind = verify_blocks(&primary, &[draft(BlockKind::Table, 0.1, "one two", 0.8)], &p);
        assert!((same_kind[0].confidence - 0.8).abs() < 1e-6);
        assert!((other_kind[0].confidence - 0.6).abs() < 1e-6, "halved support, got {}", other_kind[0].confidence);
        assert!(other_kind[0].kind == BlockKind::Paragraph, "the primary reading still owns the kind");
    }

    #[test]
    fn one_witness_cannot_vouch_for_two_blocks() {
        // A parser that split one paragraph in two must not have both halves confirmed by
        // the single block the other parser saw. The better overlap wins; the other half
        // is unwitnessed.
        let p = VerifyParams::default();
        let wide = BlockDraft { bbox: BBox::new(0.1, 0.1, 0.5, 0.2).unwrap(), ..draft(BlockKind::Paragraph, 0.1, "a b c d", 1.0) };
        let primary = vec![
            BlockDraft { bbox: BBox::new(0.1, 0.1, 0.45, 0.2).unwrap(), ..draft(BlockKind::Paragraph, 0.1, "a b c d", 0.8) },
            BlockDraft { bbox: BBox::new(0.12, 0.1, 0.5, 0.2).unwrap(), ..draft(BlockKind::Paragraph, 0.1, "a b c d", 0.8) },
        ];
        let out = verify_blocks(&primary, &[wide], &p);
        let lifted = out.iter().filter(|b| b.confidence > 0.8).count();
        assert_eq!(lifted, 1, "exactly one half is corroborated: {:?}", out.iter().map(|b| b.confidence).collect::<Vec<_>>());
        assert_eq!(out.iter().filter(|b| (b.confidence - 0.4).abs() < 1e-6).count(), 1, "the other keeps half of its own");
    }

    #[tokio::test]
    async fn verify_step_rejects_out_of_range_params() {
        let ctx = test_ctx("verify");
        let blocks = Artifact::Blocks(vec![draft(BlockKind::Paragraph, 0.1, "x", 0.5)]);
        let err = VerifyBlocks.run(&ctx, &[&blocks, &blocks], &serde_json::json!({"weight": 1.5})).await.unwrap_err();
        assert!(matches!(err, StepError::InvalidParams(_)), "{err:?}");
        // null params are the documented defaults, not an error
        let out = VerifyBlocks.run(&ctx, &[&blocks, &blocks], &serde_json::Value::Null).await.unwrap();
        let Artifact::Blocks(out) = out.artifact else { panic!("wrong kind") };
        assert!((out[0].confidence - 0.5).abs() < 1e-6);
    }
}
