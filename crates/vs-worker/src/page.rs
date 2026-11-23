//! Role `page` (Stage 1). See SPEC.md "Role page".

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use vs_classify::{combine, tier0, ClassifyConfig, PageSignals, VisionClassifier};
use vs_core::{Block, BlockContent, DocType, HashKey, JobSpec, PageProfile, PageResult, RunId};
use vs_dag::{Artifact, BlockDraft, ExecError, ExecOutput, Executor, PageImage, StepContext, StepError, StepRegistry, TextLayer};
use vs_infra::blob::{key_from_uri, keys, BlobError, BlobStore};
use vs_models::steps::crop_png;
use vs_vault::ops::{Ops, PageOutcome};
use vs_vault::VaultWriter;

use crate::dags::DagLists;
use crate::TaskError;

pub struct PageHandler {
    pub blob: Arc<dyn BlobStore>,
    pub vault: Arc<dyn VaultWriter>,
    pub ops: Arc<Ops>,
    pub dags: Arc<DagLists>,
    pub steps: Arc<StepRegistry>,
    pub classify: ClassifyConfig,
    /// Tier 1 classifier; `None` = rules only (no `stage2.classifier_model`).
    pub vision: Option<Arc<dyn VisionClassifier>>,
    pub bucket: String,
    pub render_dpi: u32,
}

pub(crate) struct PageTask<'a> {
    pub(crate) job_id: uuid::Uuid,
    pub(crate) run_id: RunId,
    pub(crate) document_hk: &'a HashKey,
    pub(crate) page_no: u32,
    pub(crate) page_image_uri: &'a str,
    pub(crate) text_layer_uri: Option<&'a str>,
    pub(crate) spec: &'a JobSpec,
    /// The document's type as the ingest stage settled it: the caller's hint, or the
    /// document classifier over the opening thumbnails. `None` leaves this page to
    /// classify itself.
    pub(crate) doc_type: Option<DocType>,
}

impl PageHandler {
    pub(crate) async fn process(&self, t: &PageTask<'_>) -> Result<(), TaskError> {
        self.ops.page_started(t.job_id, t.page_no).await?;
        let router = self.dags.router_for(&t.spec.tenant, t.spec.dag_list_version.as_deref()).await?;
        let doc = t.document_hk.as_str();

        let signals: PageSignals = parse_json(&self.blob.get(&keys::signals(doc, t.page_no)).await.map_err(page_data)?, "signals")?;
        let image_bytes = self.blob.get(key_from_uri(t.page_image_uri, &self.bucket)).await.map_err(page_data)?;
        let (width, height) = image_dimensions(&image_bytes).map_err(TaskError::permanent)?;
        let text_layer = match t.text_layer_uri {
            Some(uri) => match self.blob.get(key_from_uri(uri, &self.bucket)).await {
                Ok(b) => parse_json::<TextLayer>(&b, "text layer")?,
                Err(BlobError::NotFound(k)) => {
                    tracing::warn!(key = %k, "text layer missing; running without it");
                    TextLayer::default()
                }
                Err(e) => return Err(e.into()),
            },
            None => TextLayer::default(),
        };

        let profile = self.classify(t, &signals).await?;
        let sel = router.select(&profile, t.spec.profile);
        let dag_ref = sel.dag.reference();
        let rule_id = sel.rule_id.map(str::to_owned);
        tracing::info!(page = t.page_no, dag = %dag_ref, rule = ?rule_id, reason = ?sel.reason, doc_type = ?profile.doc_type, "routed");

        let page_hk = HashKey::page(t.document_hk, t.page_no);
        let ctx = StepContext {
            run_id: t.run_id,
            document_hk: t.document_hk.clone(),
            page_hk,
            page_no: t.page_no,
            profile: profile.clone(),
            dag_ref: dag_ref.clone(),
            node_id: String::new(),
        };
        let image = Artifact::PageImage(PageImage {
            uri: t.page_image_uri.to_string(),
            bytes: Arc::from(image_bytes.as_ref()),
            width_px: width,
            height_px: height,
            dpi: self.render_dpi,
        });
        // Kept for the heading-level pass below: the executor takes the layer by value,
        // and the spans are small next to the page image that is already in memory.
        let text_layer_spans = text_layer.spans.clone();
        let out = Executor::new(&self.steps).run(sel.dag, &ctx, image, Artifact::TextLayer(text_layer)).await.map_err(exec_error)?;

        let mut blocks = drafts_to_blocks(out, &ctx.page_hk, &t.run_id);
        // Heading levels from the type sizes in the text layer, for the headings no model
        // gave a level and no numbering could supply one. Done here because this is where
        // the text layer is: `vs-stitch` builds the section tree from `level` and never
        // sees the spans, so without this a document with unnumbered headings arrives
        // with every heading at level 1 and a section tree one level deep.
        let spans: Vec<(vs_core::BBox, f32)> =
            text_layer_spans.iter().filter_map(|s| s.font_size.map(|size| (s.bbox, size))).collect();
        let levelled = vs_stitch::page_layout::assign_heading_levels(&mut blocks, &spans);
        if levelled > 0 {
            tracing::debug!(page = t.page_no, headings = levelled, "heading levels taken from type size");
        }
        upload_crops(self.blob.as_ref(), &mut blocks, &image_bytes, &ctx.page_hk, &t.run_id).await?;
        let result = page_result(t.document_hk.clone(), t.page_no, t.run_id, profile, dag_ref.clone(), width, height, blocks);
        let body = serde_json::to_vec(&result)?;
        self.blob.put(&keys::page_result(doc, &t.run_id.to_string(), t.page_no), body.into(), "application/json").await?;
        self.vault.write_page(&result, rule_id.as_deref()).await?;

        // Recorded for the status endpoints and for the doc stage's failure threshold;
        // what used to follow — publishing the doc task when the last page landed — is
        // now simply the pipeline awaiting every page before it calls that stage.
        self.ops.page_finished(t.job_id, t.page_no, PageOutcome::Done, Some(&dag_ref), rule_id.as_deref(), None).await?;
        Ok(())
    }

    /// Tier 0 rules, then the vision classifier when configured and needed.
    ///
    /// `known_doc_type` is the document's type as the **ingest** stage settled it: the
    /// caller's hint if there was one, otherwise the document classifier shown the
    /// opening thumbnails, once per document. That is where cross-page smoothing had to
    /// go. It cannot happen here — pages fan out with no shared per-document state, so
    /// page 7 cannot learn what page 1 concluded without a round trip that would
    /// serialise the fan-out — and it cannot wait for Stage 2, which is after routing.
    /// Before the fan-out there is exactly one place holding the whole document, and the
    /// answer from there reaches every page.
    ///
    /// `None` still happens: no caller hint and no `doc_classifier_model` configured, or
    /// a classification that failed. Then this page classifies itself, as it always did.
    async fn classify(&self, t: &PageTask<'_>, signals: &PageSignals) -> Result<PageProfile, TaskError> {
        let t0 = tier0(signals, &self.classify);
        let known: Option<DocType> = t.spec.doc_type.or(t.doc_type);
        let vision = match (&self.vision, t0.needs_vision) {
            (Some(v), true) => {
                let bytes = self.blob.get(&keys::thumbnail(t.document_hk.as_str(), t.page_no)).await.map_err(page_data)?;
                let (w, h) = image_dimensions(&bytes).map_err(TaskError::permanent)?;
                let thumb = PageImage {
                    uri: keys::thumbnail(t.document_hk.as_str(), t.page_no),
                    bytes: Arc::from(bytes.as_ref()),
                    width_px: w,
                    height_px: h,
                    dpi: 0,
                };
                Some(v.classify(&thumb, &t.spec.language).await?) // model errors are transient
            }
            _ => None,
        };
        Ok(combine(t.spec.language.clone(), signals, t0, vision.as_ref(), known, &self.classify))
    }


}

/// Missing or unreadable page assets: ingest wrote them before publishing the task, so
/// this is bad page data, not a transient store error.
fn page_data(e: BlobError) -> TaskError {
    match e {
        BlobError::NotFound(_) => TaskError::permanent(e),
        BlobError::Store(_) => TaskError::from(e),
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(bytes: &Bytes, what: &str) -> Result<T, TaskError> {
    serde_json::from_slice(bytes).map_err(|e| TaskError::permanent(anyhow::anyhow!("{what}: {e}")))
}

/// Width and height from the image header only (PNG page image, JPEG thumbnail).
pub fn image_dimensions(bytes: &[u8]) -> anyhow::Result<(u32, u32)> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    Ok(reader.into_dimensions()?)
}

/// Model and I/O failures are retried; bad DAGs, bad params and unparseable model
/// output are final.
fn exec_error(e: ExecError) -> TaskError {
    let permanent = match &e {
        ExecError::Invalid(_) | ExecError::BadOutput(_) => true,
        ExecError::Timeout { .. } => false,
        ExecError::Node { source, .. } => match source {
            StepError::InvalidParams(_) | StepError::Parse(_) | StepError::NotImplemented(_) => true,
            StepError::Model(_) | StepError::Io(_) => false,
        },
    };
    if permanent {
        TaskError::permanent(e)
    } else {
        TaskError::from(e)
    }
}

/// Stores a page crop for every figure and chart block, and points the block at it.
///
/// `figure.crop` mints `crop://<page_hk>/<node>/<n>.png` for the bytes it hands down
/// the DAG, and those bytes live only for the length of the run — so what reached
/// `sat_block_content` and the graph was a URI in a scheme nothing serves. Every
/// stored figure carried a pointer no reader could follow, with `keys::crop` sitting
/// unused in `vs-infra`. This is that upload.
///
/// The crop is cut from the block's own bbox, which is the padded one `figure.crop`
/// chose where that step ran. Blocks produced by a whole-page parser never went
/// through it and have no crop at all today; they get one here too.
///
/// A failure is the page's failure: a figure block whose `crop_uri` is a lie is
/// worse than a page that retries.
pub async fn upload_crops(
    blob: &dyn BlobStore,
    blocks: &mut [Block],
    page: &Bytes,
    page_hk: &HashKey,
    run_id: &RunId,
) -> Result<(), TaskError> {
    let wanted: Vec<(usize, vs_core::BBox)> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.content, BlockContent::Figure { .. } | BlockContent::Chart { .. }))
        .map(|(i, b)| (i, b.bbox))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    // Decoding a 200 dpi page and re-encoding crops is CPU work; keep it off the
    // async executor, and decode once for all of them.
    let bytes = page.clone();
    let cropped = tokio::task::spawn_blocking(move || -> Result<Vec<(usize, Vec<u8>)>, StepError> {
        let img = image::load_from_memory(&bytes).map_err(|e| StepError::Io(format!("decode page image: {e}")))?;
        wanted
            .into_iter()
            .map(|(i, bbox)| crop_png(&img, &bbox, 0).map(|(png, _, _)| (i, png)))
            .collect()
    })
    .await
    .map_err(|e| TaskError::from(anyhow::anyhow!("crop task failed: {e}")))?
    .map_err(|e| TaskError::permanent(anyhow::anyhow!("{e}")))?;

    for (i, png) in cropped {
        let key = keys::crop(page_hk.as_str(), &run_id.to_string(), blocks[i].index);
        blob.put(&key, Bytes::from(png), "image/png").await.map_err(page_data)?;
        match &mut blocks[i].content {
            BlockContent::Figure { crop_uri, .. } | BlockContent::Chart { crop_uri, .. } => *crop_uri = key,
            _ => unreachable!("only figure and chart blocks were selected"),
        }
    }
    Ok(())
}

/// Turns executor output into final blocks: stable index by reading order, hash keys,
/// provenance taken from the trace of the node that produced each draft.
pub fn drafts_to_blocks(out: ExecOutput, page_hk: &HashKey, run_id: &RunId) -> Vec<Block> {
    let prov: HashMap<&str, &vs_core::Provenance> = out.traces.iter().map(|t| (t.node_id.as_str(), &t.provenance)).collect();
    let mut drafts: Vec<BlockDraft> = out.blocks;
    drafts.sort_by_key(|d| d.reading_order);
    drafts
        .into_iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let p = prov.get(d.node_id.as_str())?;
            let index = i as u32;
            Some(Block {
                block_hk: HashKey::block(page_hk, run_id, index),
                index,
                kind: d.kind,
                bbox: d.bbox,
                reading_order: index,
                content: d.content,
                text: d.text,
                level: d.level,
                confidence: d.confidence,
                provenance: (*p).clone(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn page_result(
    document_hk: HashKey,
    page_no: u32,
    run_id: RunId,
    profile: PageProfile,
    dag_ref: String,
    width: u32,
    height: u32,
    blocks: Vec<Block>,
) -> PageResult {
    PageResult {
        schema_version: PageResult::SCHEMA_VERSION,
        page_hk: HashKey::page(&document_hk, page_no),
        document_hk,
        page_no,
        run_id,
        render_width_px: width,
        render_height_px: height,
        profile,
        dag_ref,
        blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::{BBox, BlockKind, Provenance};
    use vs_dag::{NodeOutcome, NodeTrace};
    use vs_infra::blob::ObjectBlobStore;

    fn block(index: u32, kind: BlockKind, content: BlockContent) -> Block {
        Block {
            block_hk: HashKey::of(&[&index.to_string()]),
            index,
            kind,
            bbox: BBox::new(0.1, 0.1, 0.6, 0.6).unwrap(),
            reading_order: index,
            content,
            text: String::new(),
            level: None,
            confidence: 0.9,
            provenance: Provenance {
                run_id: RunId::new(),
                dag_ref: "d@1".into(),
                node_id: "n".into(),
                step_kind: "figure.describe".into(),
                model_ref: None,
                prompt_ref: None,
                params_hash: "0".into(),
                started_at: chrono::Utc::now(),
                duration_ms: 1,
            },
        }
    }

    /// A figure's `crop_uri` must name an object that exists.
    ///
    /// `figure.crop` hands `crop://…` down the DAG for bytes that are dropped when the
    /// run ends, so every stored figure used to point into a scheme nothing serves.
    #[tokio::test]
    async fn figure_and_chart_crops_are_stored_and_addressable() {
        let page = {
            let img = image::DynamicImage::new_rgb8(200, 100);
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
            Bytes::from(buf.into_inner())
        };
        let blob = ObjectBlobStore::new(Arc::new(object_store::memory::InMemory::new()));
        let (page_hk, run) = (HashKey::of(&["p"]), RunId::new());
        let mut blocks = vec![
            block(0, BlockKind::Paragraph, BlockContent::Text { text: "x".into() }),
            block(1, BlockKind::Figure, BlockContent::Figure { crop_uri: "crop://p/n/0.png".into(), description: None }),
            block(
                2,
                BlockKind::Chart,
                BlockContent::Chart { crop_uri: String::new(), data_html: None, description: None },
            ),
        ];
        upload_crops(&blob, &mut blocks, &page, &page_hk, &run).await.unwrap();

        // The paragraph is untouched.
        assert_eq!(blocks[0].content, BlockContent::Text { text: "x".into() });
        for i in [1, 2] {
            let uri = match &blocks[i].content {
                BlockContent::Figure { crop_uri, .. } | BlockContent::Chart { crop_uri, .. } => crop_uri.clone(),
                other => panic!("{other:?}"),
            };
            assert_eq!(uri, keys::crop(page_hk.as_str(), &run.to_string(), blocks[i].index));
            // The URI resolves, which is the whole point, and the bytes are a real PNG
            // of the block's own region: 0.5 x 0.5 of a 200 x 100 page.
            let bytes = blob.get(&uri).await.expect("crop must exist");
            let img = image::load_from_memory(&bytes).unwrap();
            assert_eq!((img.width(), img.height()), (100, 50));
        }
        // A page with nothing to crop neither decodes nor writes.
        let mut text_only = vec![block(0, BlockKind::Paragraph, BlockContent::Empty)];
        upload_crops(&blob, &mut text_only, &Bytes::from_static(b"not an image"), &page_hk, &run).await.unwrap();
    }

    #[test]
    fn drafts_get_keys_and_provenance() {
        let run = RunId::new();
        let page_hk = HashKey::of(&["p"]);
        let prov = Provenance {
            run_id: run,
            dag_ref: "d@1".into(),
            node_id: "parse".into(),
            step_kind: "page.parse_e2e".into(),
            model_ref: Some("m@1".into()),
            prompt_ref: None,
            params_hash: "0".into(),
            started_at: chrono::Utc::now(),
            duration_ms: 5,
        };
        let draft = |order: u32| BlockDraft {
            kind: BlockKind::Paragraph,
            bbox: BBox::new(0.0, 0.0, 1.0, 0.1).unwrap(),
            reading_order: order,
            content: BlockContent::Text { text: format!("b{order}") },
            text: format!("b{order}"),
            level: None,
            confidence: 0.9,
            node_id: "parse".into(),
        };
        let out = ExecOutput {
            blocks: vec![draft(5), draft(2)],
            traces: vec![NodeTrace {
                node_id: "parse".into(),
                provenance: prov,
                outcome: NodeOutcome::Ok,
                metrics: Default::default(),
            }],
        };
        let blocks = drafts_to_blocks(out, &page_hk, &run);
        assert_eq!(blocks[0].text, "b2");
        assert_eq!(blocks[1].index, 1);
        assert_eq!(blocks[1].block_hk, HashKey::block(&page_hk, &run, 1));
        assert_eq!(blocks[0].provenance.model_ref.as_deref(), Some("m@1"));
    }

    #[test]
    fn dimensions_from_png_header() {
        // 3x2 PNG, 8-bit RGB, encoded with the image crate.
        let mut png = Vec::new();
        image::RgbImage::from_pixel(3, 2, image::Rgb([0, 0, 0])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        assert_eq!(image_dimensions(&png).unwrap(), (3, 2));
        assert!(image_dimensions(b"not an image").is_err());
    }

    #[test]
    fn error_classes() {
        assert!(exec_error(ExecError::Node { node_id: "n".into(), source: StepError::Parse("x".into()) }).permanent);
        assert!(!exec_error(ExecError::Node { node_id: "n".into(), source: StepError::Model("503".into()) }).permanent);
        assert!(!exec_error(ExecError::Timeout { node_id: "n".into(), timeout_ms: 1 }).permanent);
        assert!(page_data(BlobError::NotFound("k".into())).permanent);
        assert!(!page_data(BlobError::Store("s3 down".into())).permanent);
    }
}
