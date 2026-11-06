//! Role `ingest`: see vs-ingest/SPEC.md "Ingest flow".

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use vs_core::{content_sha256, HashKey, JobSpec, JobState, RunId, Task};
use vs_infra::blob::{key_from_uri, keys, BlobStore};
use vs_infra::queue::{Lane, Role, TaskBus};
use vs_ingest::{detect_format, open_rasterizer, Format, IngestError, PageAssets};
use vs_vault::ops::Ops;
use vs_vault::{DocumentMeta, RunMeta, VaultWriter};

use crate::runtime::{Handler, TaskError};

pub struct IngestHandler {
    pub blob: Arc<dyn BlobStore>,
    pub bus: Arc<dyn TaskBus>,
    pub vault: Arc<dyn VaultWriter>,
    pub ops: Arc<Ops>,
    pub bucket: String,
    pub render_dpi: u32,
    pub thumbnail_px: u32,
    pub max_pages: u32,
}

/// Pages rendered ahead of the uploader.
const RENDER_AHEAD: usize = 4;

#[async_trait]
impl Handler for IngestHandler {
    async fn handle(&self, task: &Task) -> Result<(), TaskError> {
        let Task::Ingest { job_id, run_id, spec } = task else {
            return Err(TaskError::permanent(anyhow::anyhow!("ingest worker received {task:?}")));
        };
        self.ingest(*job_id, *run_id, spec).await
    }

    async fn give_up(&self, task: &Task, error: &str) {
        let job_id = crate::runtime::job_id(task);
        if let Err(e) = self.ops.set_state(job_id, JobState::Failed, Some(error)).await {
            tracing::error!(%job_id, error = %e, "could not mark job failed");
        }
    }
}

impl IngestHandler {
    async fn ingest(&self, job_id: uuid::Uuid, run_id: RunId, spec: &JobSpec) -> Result<(), TaskError> {
        let job = self
            .ops
            .get_job(&spec.tenant, job_id)
            .await?
            .ok_or_else(|| TaskError::permanent(anyhow::anyhow!("job {job_id} not found")))?;
        self.ops.set_state(job_id, JobState::Ingesting, None).await?;

        let original = self.blob.get(key_from_uri(&spec.source_uri, &self.bucket)).await?;
        let sha = content_sha256(&original);
        let document_hk = HashKey::document(&spec.tenant, &sha);
        let format = detect_format(&original).ok_or_else(|| TaskError::permanent(IngestError::UnsupportedFormat))?;
        tracing::info!(%document_hk, ?format, size = original.len(), "ingesting");

        let size_bytes = original.len() as i64;
        let original: Arc<Vec<u8>> = Arc::new(original.to_vec());
        let page_count = {
            let (bytes, max) = (original.clone(), self.max_pages);
            tokio::task::spawn_blocking(move || open_rasterizer(format, bytes.to_vec(), max).map(|r| r.page_count()))
                .await?
                .map_err(ingest_error)?
        };

        // Rendered pages are reused across runs: signals are written last, so their
        // presence means the page is complete.
        let mut missing = Vec::new();
        for n in 1..=page_count {
            if !self.blob.exists(&keys::signals(document_hk.as_str(), n)).await? {
                missing.push(n);
            }
        }
        tracing::info!(page_count, to_render = missing.len(), "pages");
        if !missing.is_empty() {
            self.render_and_store(format, original, &document_hk, missing).await?;
        }

        let dag_list_version = match &spec.dag_list_version {
            Some(v) => v.clone(),
            None => self.ops.active_dag_list_version(&spec.tenant).await?.unwrap_or_else(|| "none".into()),
        };
        let run = RunMeta { run_id, dag_list_version, started_at: job.created_at };
        let meta = DocumentMeta {
            document_hk: document_hk.clone(),
            content_sha256: sha,
            filename: None,
            size_bytes,
            format: format_name(format).into(),
            page_count,
        };
        self.vault.write_document(&run, spec, &meta).await?;
        self.ops.start_pages(job_id, &document_hk, page_count).await?;

        let lane = Lane::from(spec.priority);
        for n in 1..=page_count {
            let page = Task::Page {
                job_id,
                run_id,
                document_hk: document_hk.clone(),
                page_no: n,
                page_image_uri: keys::page_image(document_hk.as_str(), n, self.render_dpi),
                text_layer_uri: (format == Format::Pdf).then(|| keys::text_layer(document_hk.as_str(), n)),
                spec: spec.clone(),
            };
            self.bus.publish(Role::Page, lane, &page, &format!("{job_id}:page:{n}")).await?;
        }
        Ok(())
    }

    /// Renders on a blocking thread and uploads as pages arrive, so memory holds at most
    /// `RENDER_AHEAD` pages.
    async fn render_and_store(&self, format: Format, original: Arc<Vec<u8>>, doc: &HashKey, pages: Vec<u32>) -> Result<(), TaskError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u32, PageAssets), IngestError>>(RENDER_AHEAD);
        let (dpi, thumb, max) = (self.render_dpi, self.thumbnail_px, self.max_pages);
        let renderer = tokio::task::spawn_blocking(move || {
            let mut r = match open_rasterizer(format, original.to_vec(), max) {
                Ok(r) => r,
                Err(e) => return tx.blocking_send(Err(e)).map_err(drop),
            };
            for n in pages {
                let item = r.page(n, dpi, thumb).map(|a| (n, a));
                let failed = item.is_err();
                tx.blocking_send(item).map_err(drop)?; // receiver gone: stop
                if failed {
                    break;
                }
            }
            Ok(())
        });

        let doc = doc.as_str();
        while let Some(item) = rx.recv().await {
            let (n, a) = item.map_err(ingest_error)?;
            self.blob.put(&keys::page_image(doc, n, self.render_dpi), Bytes::from(a.image.bytes.to_vec()), "image/png").await?;
            self.blob.put(&keys::thumbnail(doc, n), Bytes::from(a.thumbnail.bytes.to_vec()), "image/jpeg").await?;
            if format == Format::Pdf {
                let layer = serde_json::to_vec(&a.text_layer)?;
                self.blob.put(&keys::text_layer(doc, n), layer.into(), "application/json").await?;
            }
            let signals = serde_json::to_vec(&a.signals)?;
            self.blob.put(&keys::signals(doc, n), signals.into(), "application/json").await?;
        }
        drop(rx);
        let _ = renderer.await?;
        Ok(())
    }
}

fn ingest_error(e: IngestError) -> TaskError {
    if e.is_permanent() {
        TaskError::permanent(e)
    } else {
        TaskError::from(e)
    }
}

fn format_name(f: Format) -> &'static str {
    match f {
        Format::Pdf => "pdf",
        Format::Png => "png",
        Format::Jpeg => "jpeg",
        Format::Tiff => "tiff",
    }
}
