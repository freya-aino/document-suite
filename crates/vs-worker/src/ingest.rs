//! Stage `ingest`: see vs-ingest/SPEC.md "Ingest flow".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use vs_core::{HashKey, JobSpec, JobState, RunId};
use vs_infra::blob::{key_from_uri, keys, BlobStore};
use vs_ingest::{detect_format, Format, IngestError, PageAssets, RenderPool};
use vs_vault::ops::Ops;
use vs_vault::{DocumentMeta, RunMeta, VaultWriter};

use crate::pipeline::PageRef;
use crate::TaskError;

pub struct IngestHandler {
    pub blob: Arc<dyn BlobStore>,
    pub vault: Arc<dyn VaultWriter>,
    pub ops: Arc<Ops>,
    /// Render child processes (pdfium never runs in the worker process).
    pub pool: Arc<RenderPool>,
    /// Originals are spooled here, one file per task.
    pub scratch_dir: PathBuf,
    pub bucket: String,
    pub render_dpi: u32,
    pub thumbnail_px: u32,
    pub max_pages: u32,
    /// How wide a newly uploaded document is open inside its organization
    /// (`api.default_visibility`). Applied once, when the document is first seen.
    pub default_visibility: vs_vault::Visibility,
}

impl IngestHandler {
    /// Renders the document and returns one [`PageRef`] per page for the page stage.
    pub(crate) async fn ingest(&self, job_id: uuid::Uuid, run_id: RunId, spec: &JobSpec) -> Result<(HashKey, Vec<PageRef>), TaskError> {
        let job = self
            .ops
            .get_job(&spec.tenant, job_id)
            .await?
            .ok_or_else(|| TaskError::permanent(anyhow::anyhow!("job {job_id} not found")))?;
        self.ops.set_state(job_id, JobState::Ingesting, None).await?;

        // Spool the original to local disk: pdfium reads it lazily from there, so
        // memory does not grow with file size.
        let spool = Spool(self.scratch_dir.join(format!("{job_id}.src")));
        let (sha, size_bytes) = self.download(key_from_uri(&spec.source_uri, &self.bucket), &spool.0).await?;
        let document_hk = HashKey::document(&spec.tenant, &sha);
        let format = sniff_format(&spool.0).await?.ok_or_else(|| TaskError::permanent(IngestError::UnsupportedFormat))?;
        tracing::info!(%document_hk, ?format, size = size_bytes, "ingesting");

        let page_count = self.pool.page_count(&spool.0, format, self.max_pages).await.map_err(ingest_error)?;

        // Rendered pages are reused across runs: signals are written last, so their
        // presence means the page is complete.
        let mut missing = Vec::new();
        for n in 1..=page_count {
            if !self.blob.exists(&keys::signals(document_hk.as_str(), n)).await? {
                missing.push(n);
            }
        }
        tracing::info!(page_count, to_render = missing.len(), "pages");
        self.render_and_store(format, &spool.0, &document_hk, missing).await?;

        let dag_list_version = match &spec.dag_list_version {
            Some(v) => v.clone(),
            None => self.ops.active_dag_list_version(&spec.tenant).await?.unwrap_or_else(|| "none".into()),
        };
        let run = RunMeta { run_id, dag_list_version, started_at: job.created_at };
        let meta = DocumentMeta {
            document_hk: document_hk.clone(),
            content_sha256: sha,
            filename: None,
            size_bytes: size_bytes as i64,
            format: format.as_str().into(),
            page_count,
        };
        self.vault.write_document(&run, spec, &meta).await?;
        // Who may read this document, recorded here because this is the first point at
        // which the document hash key exists. It does not overwrite: re-running a
        // document keeps the first uploader and whatever has been shared since.
        if self.ops.record_document_owner(&spec.tenant, &document_hk, spec.owner.as_ref(), self.default_visibility).await? {
            tracing::debug!(%document_hk, visibility = self.default_visibility.as_str(), "recorded document ownership");
        }
        self.ops.start_pages(job_id, &document_hk, page_count).await?;

        let pages = (1..=page_count)
            .map(|n| PageRef {
                page_no: n,
                page_image_uri: keys::page_image(document_hk.as_str(), n, self.render_dpi),
                text_layer_uri: (format == Format::Pdf).then(|| keys::text_layer(document_hk.as_str(), n)),
            })
            .collect();
        Ok((document_hk, pages))
    }

    /// Streams the object to `path`, hashing on the way. Returns (sha256 hex, size).
    async fn download(&self, key: &str, path: &Path) -> Result<(String, u64), TaskError> {
        let mut stream = self.blob.get_stream(key).await?;
        let mut file = tokio::fs::File::create(path).await?;
        let (mut hasher, mut size) = (Sha256::new(), 0u64);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            size += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok((hex::encode(hasher.finalize()), size))
    }

    /// Renders pages in parallel across the pool's children and uploads each page as it
    /// arrives; memory holds at most one page per child.
    async fn render_and_store(&self, format: Format, path: &Path, doc: &HashKey, pages: Vec<u32>) -> Result<(), TaskError> {
        let (dpi, thumb, max) = (self.render_dpi, self.thumbnail_px, self.max_pages);
        futures::stream::iter(pages)
            .map(|n| async move { self.pool.page(path, format, max, n, dpi, thumb).await.map(|a| (n, a)).map_err(ingest_error) })
            .buffer_unordered(self.pool.size())
            .map(|r| async move {
                let (n, a) = r?;
                self.store_page(format, doc.as_str(), n, a).await
            })
            .buffer_unordered(self.pool.size())
            .try_collect::<Vec<()>>()
            .await?;
        Ok(())
    }

    async fn store_page(&self, format: Format, doc: &str, n: u32, a: PageAssets) -> Result<(), TaskError> {
        self.blob.put(&keys::page_image(doc, n, self.render_dpi), Bytes::from(a.image.bytes.to_vec()), "image/png").await?;
        self.blob.put(&keys::thumbnail(doc, n), Bytes::from(a.thumbnail.bytes.to_vec()), "image/jpeg").await?;
        if format == Format::Pdf {
            let layer = serde_json::to_vec(&a.text_layer)?;
            self.blob.put(&keys::text_layer(doc, n), layer.into(), "application/json").await?;
        }
        let signals = serde_json::to_vec(&a.signals)?;
        self.blob.put(&keys::signals(doc, n), signals.into(), "application/json").await?;
        Ok(())
    }
}

/// Deletes the spooled original when the task ends, however it ends.
struct Spool(PathBuf);

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Format from the first KiB of the file (magic bytes only).
async fn sniff_format(path: &Path) -> std::io::Result<Option<Format>> {
    let mut head = Vec::with_capacity(1024);
    tokio::fs::File::open(path).await?.take(1024).read_to_end(&mut head).await?;
    Ok(detect_format(&head))
}

fn ingest_error(e: IngestError) -> TaskError {
    if e.is_permanent() {
        TaskError::permanent(e)
    } else {
        TaskError::from(e)
    }
}
