//! CPU-only DAG steps.

use async_trait::async_trait;

use ds_core::{BBox, BlockContent, BlockKind};
use ds_dag::{Artifact, ArtifactKind, BlockDraft, LayoutRegion, PageImage, Signature, Step, StepContext, StepError, StepOutput, TextLayer};

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
    /// `worker.max_crop_dpi = 0`, an original past its retention, an offline `ds-adapt`
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
pub fn crop_figures(page_bytes: &[u8], layout: &[LayoutRegion], padding: f32, uri_prefix: &str) -> Result<Vec<ds_dag::FigureCrop>, StepError> {
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
        out.push(ds_dag::FigureCrop {
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

/// `blocks, blocks, blocks → blocks`
pub struct VoteBlocks;

/// How three readings of a page are reconciled.
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
pub struct VoteParams {
    /// Overlap at which blocks from two readings count as the *same* block. Matches
    /// `blocks.merge` and `blocks.verify`, so the three steps agree about identity.
    pub iou: f32,
    /// How many of the three readings must have seen a block for it to survive.
    pub min_votes: u32,
    /// Whether an output confidence is scaled by the share of readings that voted for
    /// the block. On by default: a block two of three parsers saw is not as certain as
    /// one all three saw, and nothing downstream can recover that from the membership.
    pub weight_by_votes: bool,
}

impl Default for VoteParams {
    fn default() -> Self {
        Self { iou: 0.5, min_votes: 2, weight_by_votes: true }
    }
}

#[async_trait]
impl Step for VoteBlocks {
    fn kind(&self) -> &'static str {
        "blocks.vote"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::Blocks, ArtifactKind::Blocks, ArtifactKind::Blocks], output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        5_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let mut readings: Vec<&[BlockDraft]> = Vec::with_capacity(inputs.len());
        for a in inputs {
            let Artifact::Blocks(b) = a else {
                return Err(StepError::InvalidParams("unexpected input kinds".into()));
            };
            readings.push(b);
        }
        let p: VoteParams =
            if params.is_null() { VoteParams::default() } else { serde_json::from_value(params.clone()).map_err(|e| StepError::InvalidParams(e.to_string()))? };
        if !(0.0..=1.0).contains(&p.iou) || p.iou.is_nan() {
            return Err(StepError::InvalidParams(format!("iou must be within 0..=1, got {}", p.iou)));
        }
        // 0 would keep everything and `> readings` would keep nothing, and both read as
        // "the step did nothing" from the outside.
        if p.min_votes == 0 || p.min_votes as usize > readings.len() {
            return Err(StepError::InvalidParams(format!("min_votes must be within 1..={}, got {}", readings.len(), p.min_votes)));
        }
        Ok(StepOutput::plain(Artifact::Blocks(vote_blocks(&readings, &p))))
    }
}

/// Keeps the blocks that enough independent readings of the page agree on.
///
/// This is the thing two-input `blocks.verify` structurally cannot do. Verify re-scores
/// the primary reading and never changes membership, so a block the primary missed stays
/// missing however many other parsers saw it, and a block only the primary saw stays
/// present with a reduced confidence. A vote decides membership *by agreement*: with
/// `min_votes = 2` a block two parsers saw is kept even if the primary missed it, and a
/// block only one saw is dropped.
///
/// Identity is positional (IoU), and a block in one reading may be matched by at most one
/// block from each other reading — otherwise a parser that split a paragraph in two would
/// cast two votes for the same region.
pub fn vote_blocks(readings: &[&[BlockDraft]], p: &VoteParams) -> Vec<BlockDraft> {
    struct Cluster {
        members: Vec<BlockDraft>,
        /// Which readings have already voted, so one cannot vote twice.
        voters: Vec<usize>,
    }
    let mut clusters: Vec<Cluster> = Vec::new();

    for (r, reading) in readings.iter().enumerate() {
        for block in reading.iter() {
            // Highest overlap first, among the clusters this reading has not voted for.
            let best = clusters
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.voters.contains(&r))
                .filter_map(|(i, c)| {
                    let iou = c.members.iter().map(|m| m.bbox.intersection_over_union(&block.bbox)).fold(0.0f32, f32::max);
                    (iou >= p.iou).then_some((i, iou))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i);
            match best {
                Some(i) => {
                    clusters[i].members.push(block.clone());
                    clusters[i].voters.push(r);
                }
                None => clusters.push(Cluster { members: vec![block.clone()], voters: vec![r] }),
            }
        }
    }

    let n = readings.len() as f32;
    let mut out: Vec<BlockDraft> = clusters
        .iter()
        .filter(|c| c.voters.len() as u32 >= p.min_votes)
        .map(|c| {
            // The most confident reading supplies the geometry and the text: averaging
            // two boxes would invent a region no parser reported, and averaging two
            // strings is not defined.
            let rep = c.members.iter().max_by(|a, b| a.confidence.total_cmp(&b.confidence)).expect("a cluster has a member");
            let mut block = rep.clone();
            // The kind is the majority's, which is the one field a vote can decide
            // without inventing anything: three readings of a region are three opinions
            // about what it is.
            let mut tally: Vec<(BlockKind, usize)> = Vec::new();
            for m in &c.members {
                match tally.iter_mut().find(|(k, _)| *k == m.kind) {
                    Some((_, n)) => *n += 1,
                    None => tally.push((m.kind, 1)),
                }
            }
            let top = tally.iter().map(|(_, n)| *n).max().unwrap_or(0);
            // A tie is not a majority, so the representative keeps its own kind rather
            // than the first one encountered winning by list order.
            if tally.iter().filter(|(_, n)| *n == top).count() == 1 {
                block.kind = tally.iter().find(|(_, n)| *n == top).expect("a top kind").0;
            }
            let mean = c.members.iter().map(|m| m.confidence).sum::<f32>() / c.members.len() as f32;
            block.confidence = if p.weight_by_votes { mean * (c.voters.len() as f32 / n) } else { mean };
            block
        })
        .collect();

    out.sort_by_key(|b| b.reading_order);
    out
}

/// `blocks → blocks`
pub struct ReorderBlocks;

/// How reading order is recovered from geometry.
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
pub struct ReorderParams {
    /// A block at least this wide (as a share of the page) is treated as *spanning* —
    /// a title or a full-width table that belongs between the column groups rather than
    /// inside one.
    pub full_width: f32,
    /// Normalised horizontal gap below which two blocks are taken to share a column.
    /// Columns are found by overlap of the blocks' own x-extents, and this widens each
    /// extent so that a short line does not start a column of its own.
    pub gutter: f32,
}

impl Default for ReorderParams {
    fn default() -> Self {
        Self { full_width: 0.6, gutter: 0.02 }
    }
}

#[async_trait]
impl Step for ReorderBlocks {
    fn kind(&self) -> &'static str {
        "blocks.reorder"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::Blocks], output: ArtifactKind::Blocks }
    }
    fn default_timeout_ms(&self) -> u64 {
        2_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let Artifact::Blocks(blocks) = inputs[0] else {
            return Err(StepError::InvalidParams("unexpected input kinds".into()));
        };
        let p: ReorderParams =
            if params.is_null() { ReorderParams::default() } else { serde_json::from_value(params.clone()).map_err(|e| StepError::InvalidParams(e.to_string()))? };
        for (name, v) in [("full_width", p.full_width), ("gutter", p.gutter)] {
            if !(0.0..=1.0).contains(&v) || v.is_nan() {
                return Err(StepError::InvalidParams(format!("{name} must be within 0..=1, got {v}")));
            }
        }
        Ok(StepOutput::plain(Artifact::Blocks(reorder_blocks(blocks, &p))))
    }
}

/// Recovers reading order from the blocks' geometry, changing nothing else.
///
/// The only step in the catalogue that can move an order metric without changing the
/// parse: membership, boxes, text and kinds come out exactly as they went in, and only
/// `reading_order` is rewritten. That is what makes it measurable — a change in the order
/// score after inserting it is attributable to it alone.
///
/// An XY-cut with one simplification that matters on real pages: a block spanning most of
/// the width (a title, a full-width table) cuts the page into horizontal stripes, and
/// column grouping happens *within* a stripe. Without that, a spanning block's x-extent
/// overlaps every column and collapses the page into a single column, which is the
/// classic way a naive column sort reads a two-column page with a headline straight down
/// the middle.
pub fn reorder_blocks(blocks: &[BlockDraft], p: &ReorderParams) -> Vec<BlockDraft> {
    // Indices throughout, so the output is a permutation of the input by construction.
    let spanning: Vec<usize> = (0..blocks.len()).filter(|&i| blocks[i].bbox.x1 - blocks[i].bbox.x0 >= p.full_width).collect();
    let body: Vec<usize> = (0..blocks.len()).filter(|&i| !spanning.contains(&i)).collect();

    // Stripe boundaries: the top edge of each spanning block, in page order.
    let mut cuts: Vec<f32> = spanning.iter().map(|&i| blocks[i].bbox.y0).collect();
    cuts.sort_by(|a, b| a.total_cmp(b));

    let stripe_of = |y: f32| cuts.iter().filter(|&&c| c <= y).count();

    let mut order: Vec<usize> = Vec::with_capacity(blocks.len());
    // Stripe 0 is above the first spanning block; stripe k is below the k-th one.
    for stripe in 0..=cuts.len() {
        if stripe > 0 {
            // The spanning block that opened this stripe comes first.
            let y = cuts[stripe - 1];
            let mut openers: Vec<usize> = spanning.iter().copied().filter(|&i| blocks[i].bbox.y0 == y).collect();
            openers.sort_by(|&a, &b| blocks[a].bbox.x0.total_cmp(&blocks[b].bbox.x0));
            order.extend(openers);
        }
        let mut here: Vec<usize> = body.iter().copied().filter(|&i| stripe_of(blocks[i].bbox.y0) == stripe).collect();
        if here.is_empty() {
            continue;
        }
        // Columns: x-extents widened by half a gutter each side, grouped transitively by
        // overlap. Left to right.
        here.sort_by(|&a, &b| blocks[a].bbox.x0.total_cmp(&blocks[b].bbox.x0));
        let mut columns: Vec<(f32, Vec<usize>)> = Vec::new();
        for &i in &here {
            let (x0, x1) = (blocks[i].bbox.x0 - p.gutter / 2.0, blocks[i].bbox.x1 + p.gutter / 2.0);
            match columns.last_mut().filter(|(right, _)| x0 <= *right) {
                Some((right, members)) => {
                    *right = right.max(x1);
                    members.push(i);
                }
                None => columns.push((x1, vec![i])),
            }
        }
        for (_, mut members) in columns {
            members.sort_by(|&a, &b| blocks[a].bbox.y0.total_cmp(&blocks[b].bbox.y0).then(blocks[a].bbox.x0.total_cmp(&blocks[b].bbox.x0)));
            order.extend(members);
        }
    }

    order
        .into_iter()
        .enumerate()
        .map(|(n, i)| BlockDraft { reading_order: n as u32, ..blocks[i].clone() })
        .collect()
}

/// `layout, layout → layout`
pub struct MergeLayout;

/// How two layout readings are unioned.
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
pub struct MergeLayoutParams {
    /// Overlap at which two regions are the same region.
    pub iou: f32,
}

impl Default for MergeLayoutParams {
    fn default() -> Self {
        Self { iou: 0.5 }
    }
}

#[async_trait]
impl Step for MergeLayout {
    fn kind(&self) -> &'static str {
        "layout.merge"
    }
    fn signature(&self) -> Signature {
        Signature { inputs: vec![ArtifactKind::Layout, ArtifactKind::Layout], output: ArtifactKind::Layout }
    }
    fn default_timeout_ms(&self) -> u64 {
        2_000
    }
    async fn run(&self, _ctx: &StepContext, inputs: &[&Artifact], params: &serde_json::Value) -> Result<StepOutput, StepError> {
        let (Artifact::Layout(a), Artifact::Layout(b)) = (inputs[0], inputs[1]) else {
            return Err(StepError::InvalidParams("unexpected input kinds".into()));
        };
        let p: MergeLayoutParams = if params.is_null() {
            MergeLayoutParams::default()
        } else {
            serde_json::from_value(params.clone()).map_err(|e| StepError::InvalidParams(e.to_string()))?
        };
        if !(0.0..=1.0).contains(&p.iou) || p.iou.is_nan() {
            return Err(StepError::InvalidParams(format!("iou must be within 0..=1, got {}", p.iou)));
        }
        Ok(StepOutput::plain(Artifact::Layout(merge_layout(a, b, &p))))
    }
}

/// Unions two layout readings, the better-scored region winning where they overlap.
///
/// The counterpart of `blocks.merge` one artifact earlier. Merging *layouts* and then
/// parsing once is not the same pipeline as parsing twice and merging blocks: it pays for
/// one parse instead of two, and a region only the second detector saw still gets read.
/// Which of the two is better is a question for the search, and until this step existed it
/// could not be asked.
pub fn merge_layout(a: &[LayoutRegion], b: &[LayoutRegion], p: &MergeLayoutParams) -> Vec<LayoutRegion> {
    let mut out = a.to_vec();
    for nr in b {
        match out.iter_mut().find(|x| x.bbox.intersection_over_union(&nr.bbox) >= p.iou) {
            Some(existing) if nr.score > existing.score => {
                // The order is the primary reading's: a detector's own numbering is not
                // comparable across detectors, and `blocks.reorder` is the step that
                // exists to decide order on purpose.
                *existing = LayoutRegion { reading_order: existing.reading_order, ..nr.clone() };
            }
            Some(_) => {}
            None => out.push(nr.clone()),
        }
    }
    out.sort_by_key(|r| r.reading_order);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::BBox;
    use ds_dag::TextSpan;

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
        use ds_core::{ClassifierTier, DocType, HashKey, LanguageTag, PageProfile, RunId};
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
        let page = Artifact::PageImage(ds_dag::PageImage { uri: "mem".into(), bytes: Arc::from(png.into_boxed_slice()), width_px: w, height_px: h, dpi: 72 });
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

    /// A block at an arbitrary place, for the geometry-only steps.
    fn at(x0: f32, y0: f32, x1: f32, y1: f32, order: u32) -> BlockDraft {
        BlockDraft {
            kind: BlockKind::Paragraph,
            bbox: BBox::new(x0, y0, x1, y1).unwrap(),
            reading_order: order,
            content: BlockContent::Text { text: format!("{x0},{y0}") },
            text: format!("{x0},{y0}"),
            level: None,
            confidence: 0.8,
            node_id: "n".into(),
        }
    }

    #[test]
    fn a_vote_decides_membership_where_verify_cannot() {
        // The point of the step, stated as a test: the primary missed a block two other
        // readings saw, and it is kept; the primary saw a block nobody else did, and it
        // is dropped. `blocks.verify` can do neither — it only re-scores.
        let p = VoteParams::default();
        let shared = draft(BlockKind::Paragraph, 0.1, "everyone saw this", 0.9);
        let primary_only = draft(BlockKind::Paragraph, 0.5, "only the primary", 0.9);
        let missed = draft(BlockKind::Paragraph, 0.75, "the primary missed this", 0.6);
        let out = vote_blocks(&[&[shared.clone(), primary_only], &[shared.clone(), missed.clone()], &[shared.clone(), missed]], &p);
        let texts: Vec<&str> = out.iter().map(|b| b.text.as_str()).collect();
        assert!(texts.contains(&"everyone saw this"));
        assert!(texts.contains(&"the primary missed this"), "two votes is a majority: {texts:?}");
        assert!(!texts.contains(&"only the primary"), "one vote is not: {texts:?}");
    }

    #[test]
    fn a_vote_is_weighted_by_how_many_saw_it() {
        let p = VoteParams::default();
        let b = draft(BlockKind::Paragraph, 0.1, "x", 0.9);
        let one = std::slice::from_ref(&b);
        let unanimous = vote_blocks(&[one, one, one], &p);
        assert!((unanimous[0].confidence - 0.9).abs() < 1e-6, "3/3 keeps its confidence");
        let two_thirds = vote_blocks(&[one, one, &[]], &p);
        assert!((two_thirds[0].confidence - 0.6).abs() < 1e-6, "2/3 of 0.9, got {}", two_thirds[0].confidence);
        // Off, the membership decision stays and the scaling goes — the knob exists
        // because a downstream threshold tuned on a parser's own confidences should not
        // have to be retuned to insert a vote.
        let plain = vote_blocks(&[one, one, &[]], &VoteParams { weight_by_votes: false, ..VoteParams::default() });
        assert!((plain[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn one_reading_cannot_cast_two_votes_for_a_region() {
        // A parser that split one paragraph in two must not thereby elect it alone.
        let p = VoteParams::default();
        let whole = at(0.1, 0.1, 0.5, 0.3, 0);
        let split_a = at(0.1, 0.1, 0.5, 0.2, 0);
        let split_b = at(0.1, 0.11, 0.5, 0.21, 1);
        let out = vote_blocks(&[&[split_a, split_b], &[], &[]], &p);
        assert!(out.is_empty(), "two halves from one parser are one vote each, not a majority: {out:?}");
        // And with a genuine second witness the region survives.
        let out = vote_blocks(&[&[at(0.1, 0.1, 0.5, 0.3, 0)], &[whole], &[]], &p);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_vote_takes_the_majority_kind_but_not_a_tie() {
        let p = VoteParams::default();
        let table = BlockDraft { kind: BlockKind::Table, ..draft(BlockKind::Table, 0.1, "x", 0.5) };
        let para = draft(BlockKind::Paragraph, 0.1, "x", 0.9);
        // Two tables against one paragraph: the paragraph is the most confident reading
        // and so supplies text and geometry, but the kind is the majority's.
        let (one_para, one_table) = (std::slice::from_ref(&para), std::slice::from_ref(&table));
        let out = vote_blocks(&[one_para, one_table, one_table], &p);
        assert_eq!(out[0].kind, BlockKind::Table);
        assert_eq!(out[0].text, "x");
        // One each is no majority, so the representative keeps its own kind rather than
        // list order deciding.
        let out = vote_blocks(&[one_para, one_table], &VoteParams { min_votes: 2, ..VoteParams::default() });
        assert_eq!(out[0].kind, BlockKind::Paragraph, "a tie is not a majority");
    }

    #[tokio::test]
    async fn the_vote_step_rejects_a_quorum_it_cannot_reach() {
        let ctx = test_ctx("vote");
        let blocks = Artifact::Blocks(vec![draft(BlockKind::Paragraph, 0.1, "x", 0.5)]);
        for bad in [serde_json::json!({"min_votes": 0}), serde_json::json!({"min_votes": 4}), serde_json::json!({"iou": 1.5})] {
            let err = VoteBlocks.run(&ctx, &[&blocks, &blocks, &blocks], &bad).await.unwrap_err();
            assert!(matches!(err, StepError::InvalidParams(_)), "{bad} gave {err:?}");
        }
        assert!(VoteBlocks.run(&ctx, &[&blocks, &blocks, &blocks], &serde_json::Value::Null).await.is_ok());
    }

    #[test]
    fn reorder_reads_two_columns_down_and_not_across() {
        // Interleaved on the page, the way a parser that sorted by y alone would emit
        // them: left-top, right-top, left-bottom, right-bottom.
        let blocks = vec![
            at(0.08, 0.20, 0.45, 0.30, 0),
            at(0.55, 0.21, 0.92, 0.31, 1),
            at(0.08, 0.40, 0.45, 0.50, 2),
            at(0.55, 0.41, 0.92, 0.51, 3),
        ];
        let out = reorder_blocks(&blocks, &ReorderParams::default());
        let xs: Vec<f32> = out.iter().map(|b| b.bbox.x0).collect();
        assert_eq!(xs, vec![0.08, 0.08, 0.55, 0.55], "a column is read to its foot before the next one starts: {xs:?}");
        assert_eq!(out.iter().map(|b| b.reading_order).collect::<Vec<_>>(), vec![0, 1, 2, 3], "the order is renumbered densely");
    }

    #[test]
    fn a_headline_cuts_the_page_rather_than_collapsing_the_columns() {
        // The failure this step's stripe logic exists for: a full-width title overlaps
        // both columns' x-extents, so a naive column sort finds one column and reads the
        // page straight down the middle.
        let blocks = vec![
            at(0.05, 0.05, 0.95, 0.12, 0),
            at(0.08, 0.20, 0.45, 0.30, 1),
            at(0.55, 0.21, 0.92, 0.31, 2),
            at(0.05, 0.60, 0.95, 0.66, 3),
            at(0.08, 0.70, 0.45, 0.80, 4),
            at(0.55, 0.71, 0.92, 0.81, 5),
        ];
        let out = reorder_blocks(&blocks, &ReorderParams::default());
        let ys: Vec<f32> = out.iter().map(|b| b.bbox.y0).collect();
        assert_eq!(ys, vec![0.05, 0.20, 0.21, 0.60, 0.70, 0.71], "title, its two columns, second title, its two columns: {ys:?}");
    }

    #[test]
    fn reorder_changes_nothing_but_the_order() {
        let blocks = vec![at(0.55, 0.21, 0.92, 0.31, 0), at(0.08, 0.20, 0.45, 0.30, 1)];
        let out = reorder_blocks(&blocks, &ReorderParams::default());
        // A permutation: same multiset of everything that is not the order. This is the
        // property that makes a change in the order metric attributable to this step.
        let key = |b: &BlockDraft| (b.text.clone(), format!("{:?}", b.kind), b.bbox.x0.to_bits(), b.confidence.to_bits());
        let mut before: Vec<_> = blocks.iter().map(key).collect();
        let mut after: Vec<_> = out.iter().map(key).collect();
        before.sort();
        after.sort();
        assert_eq!(before, after);
        assert_eq!(out[0].bbox.x0, 0.08, "and it did reorder them");
    }

    #[test]
    fn merging_layouts_unions_and_the_better_score_wins() {
        let a = vec![
            LayoutRegion { kind: BlockKind::Paragraph, bbox: BBox::new(0.1, 0.1, 0.5, 0.2).unwrap(), reading_order: 0, score: 0.6 },
            LayoutRegion { kind: BlockKind::Paragraph, bbox: BBox::new(0.1, 0.3, 0.5, 0.4).unwrap(), reading_order: 1, score: 0.9 },
        ];
        let b = vec![
            // The same first region, read as a table and more confidently.
            LayoutRegion { kind: BlockKind::Table, bbox: BBox::new(0.1, 0.1, 0.5, 0.2).unwrap(), reading_order: 7, score: 0.95 },
            // One only the second detector saw.
            LayoutRegion { kind: BlockKind::Figure, bbox: BBox::new(0.6, 0.6, 0.9, 0.9).unwrap(), reading_order: 9, score: 0.5 },
        ];
        let out = merge_layout(&a, &b, &MergeLayoutParams::default());
        assert_eq!(out.len(), 3, "union, not replacement: {out:?}");
        let first = out.iter().find(|r| r.bbox.y0 == 0.1).expect("the shared region");
        assert_eq!(first.kind, BlockKind::Table, "the better-scored reading wins");
        assert_eq!(first.reading_order, 0, "but not its order — orders are not comparable across detectors");
        assert!(out.iter().any(|r| r.kind == BlockKind::Figure), "a region only the second saw is kept");
    }
}
