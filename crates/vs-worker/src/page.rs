//! Role `page` (Stage 1). See SPEC.md "Role page".

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use vs_classify::{combine, tier0, ClassifyConfig, PageSignals, VisionClassifier};
use vs_core::{Block, DocType, HashKey, JobSpec, PageProfile, PageResult, RunId, Task};
use vs_dag::{Artifact, BlockDraft, ExecError, ExecOutput, Executor, PageImage, StepContext, StepError, StepRegistry, TextLayer};
use vs_infra::blob::{key_from_uri, keys, BlobError, BlobStore};
use vs_infra::queue::{Lane, Role, TaskBus};
use vs_vault::ops::{Ops, PageOutcome, PageProgress};
use vs_vault::VaultWriter;

use crate::dags::DagLists;
use crate::runtime::{Handler, TaskError};

pub struct PageHandler {
    pub blob: Arc<dyn BlobStore>,
    pub bus: Arc<dyn TaskBus>,
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

#[async_trait]
impl Handler for PageHandler {
    async fn handle(&self, task: &Task) -> Result<(), TaskError> {
        let Task::Page { job_id, run_id, document_hk, page_no, page_image_uri, text_layer_uri, spec } = task else {
            return Err(TaskError::permanent(anyhow::anyhow!("page worker received {task:?}")));
        };
        let page = PageTask {
            job_id: *job_id,
            run_id: *run_id,
            document_hk,
            page_no: *page_no,
            page_image_uri,
            text_layer_uri: text_layer_uri.as_deref(),
            spec,
        };
        self.process(&page).await
    }

    /// A page that exhausted its attempts counts as failed; the same completion check
    /// runs so a failed last page still enqueues the doc task.
    async fn give_up(&self, task: &Task, error: &str) {
        let Task::Page { job_id, page_no, spec, .. } = task else {
            return;
        };
        match self.ops.page_finished(*job_id, *page_no, PageOutcome::Failed, None, None, Some(error)).await {
            Ok(progress) => {
                if let Err(e) = self.maybe_enqueue_doc(task, progress).await {
                    tracing::error!(%job_id, error = %format!("{:#}", e.error), "could not enqueue doc task after failed page");
                }
            }
            Err(e) => {
                tracing::error!(%job_id, page_no, tenant = spec.tenant.as_str(), error = %e, "could not mark page failed")
            }
        }
    }
}

struct PageTask<'a> {
    job_id: uuid::Uuid,
    run_id: RunId,
    document_hk: &'a HashKey,
    page_no: u32,
    page_image_uri: &'a str,
    text_layer_uri: Option<&'a str>,
    spec: &'a JobSpec,
}

impl PageHandler {
    async fn process(&self, t: &PageTask<'_>) -> Result<(), TaskError> {
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
        let out = Executor::new(&self.steps).run(sel.dag, &ctx, image, Artifact::TextLayer(text_layer)).await.map_err(exec_error)?;

        let blocks = drafts_to_blocks(out, &ctx.page_hk, &t.run_id);
        let result = page_result(t.document_hk.clone(), t.page_no, t.run_id, profile, dag_ref.clone(), width, height, blocks);
        let body = serde_json::to_vec(&result)?;
        self.blob.put(&keys::page_result(doc, &t.run_id.to_string(), t.page_no), body.into(), "application/json").await?;
        self.vault.write_page(&result, rule_id.as_deref()).await?;

        let progress = self.ops.page_finished(t.job_id, t.page_no, PageOutcome::Done, Some(&dag_ref), rule_id.as_deref(), None).await?;
        self.maybe_enqueue_doc_spec(t.job_id, t.run_id, t.document_hk, t.spec, progress).await
    }

    /// Tier 0 rules, then the vision classifier when configured and needed. No
    /// doc-level smoothing: pages are independent in v1 (`known_doc_type = None`).
    async fn classify(&self, t: &PageTask<'_>, signals: &PageSignals) -> Result<PageProfile, TaskError> {
        let t0 = tier0(signals, &self.classify);
        let known: Option<DocType> = None;
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

    async fn maybe_enqueue_doc(&self, task: &Task, progress: PageProgress) -> Result<(), TaskError> {
        let Task::Page { job_id, run_id, document_hk, spec, .. } = task else {
            return Ok(());
        };
        self.maybe_enqueue_doc_spec(*job_id, *run_id, document_hk, spec, progress).await
    }

    /// Publishes the doc task once every page is final. `all_finished()` rather than
    /// `completed_job()`: a redelivered page task whose earlier attempt counted the page
    /// but died before publishing must still publish, or the job never leaves
    /// `pages_processing`. The duplicate publish of a healthy retry is absorbed by the
    /// JetStream de-dup id (`<job_id>:doc`); beyond the de-dup window Stage 2 is
    /// idempotent per run.
    async fn maybe_enqueue_doc_spec(
        &self,
        job_id: uuid::Uuid,
        run_id: RunId,
        document_hk: &HashKey,
        spec: &JobSpec,
        progress: PageProgress,
    ) -> Result<(), TaskError> {
        if !progress.all_finished() {
            return Ok(());
        }
        tracing::info!(%job_id, done = progress.done, failed = progress.failed, counted = progress.counted, "all pages finished; enqueueing doc task");
        let task = Task::Doc { job_id, run_id, document_hk: document_hk.clone(), spec: spec.clone() };
        self.bus.publish(Role::Doc, Lane::from(spec.priority), &task, &format!("{job_id}:doc")).await?;
        Ok(())
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
    use vs_core::{BBox, BlockContent, BlockKind, Provenance};
    use vs_dag::{NodeOutcome, NodeTrace};

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
            traces: vec![NodeTrace { node_id: "parse".into(), provenance: prov, outcome: NodeOutcome::Ok }],
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
