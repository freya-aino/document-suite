//! Stage `ingest`: see ds-ingest/SPEC.md "Ingest flow".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use ds_classify::DocumentClassifier;
use ds_core::{DocTypeSource, HashKey, JobSpec, JobState, RunId};
use ds_dag::PageImage;
use ds_infra::blob::{key_from_uri, keys, BlobStore};
use ds_ingest::{detect_format, Format, IngestError, PageAssets, RenderPool};
use ds_vault::ops::Ops;
use ds_vault::{DocumentMeta, RunMeta, VaultWriter};

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
    pub default_visibility: ds_vault::Visibility,
    /// Classifies the whole document from its opening thumbnails, once, before any page
    /// is routed.
    ///
    /// This is the cross-page smoothing `ds-classify` asked for, in the only place it can
    /// go. Pages fan out with no shared per-document state, so page 7 cannot learn what
    /// page 1 concluded — but *before* the fan-out there is one place that has the whole
    /// document in hand, and this is it. `None` leaves every page routed on its own
    /// classification, which is what happened before.
    pub doc_classifier: Option<Arc<dyn DocumentClassifier>>,
    /// Opening pages the document classifier is shown. Three is enough to tell an invoice
    /// from a paper and cheap enough to pay for on every document.
    pub doc_classify_pages: usize,
}

/// What ingest found out about the document before any page was routed.
pub(crate) struct Ingested {
    pub(crate) document_hk: HashKey,
    pub(crate) pages: Vec<PageRef>,
    /// The document's type, when it could be settled up front: the caller's hint, or the
    /// document classifier over the opening thumbnails. Every page is then routed knowing
    /// what document it belongs to.
    pub(crate) doc_type: Option<ds_core::DocumentType>,
}

impl IngestHandler {
    /// Renders the document and returns what the page stage needs to run it.
    pub(crate) async fn ingest(&self, job_id: uuid::Uuid, run_id: RunId, spec: &JobSpec) -> Result<Ingested, TaskError> {
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
        let doc_type = self.document_type(spec, &document_hk, page_count).await;
        Ok(Ingested { document_hk, pages, doc_type })
    }

    /// The document's type, settled before any page is routed.
    ///
    /// The caller's hint wins outright — someone uploading a batch of invoices knows more
    /// than a model looking at pixels. Otherwise the document classifier is shown the
    /// first few thumbnails, which is one small model call per *document* rather than per
    /// page, and the answer reaches every page's routing.
    ///
    /// Failure is not fatal and not retried: the pages fall back to classifying
    /// themselves, which is exactly what they did before this existed. A document is not
    /// worth failing over a type hint.
    async fn document_type(&self, spec: &JobSpec, document_hk: &HashKey, page_count: u32) -> Option<ds_core::DocumentType> {
        if let Some(doc_type) = spec.doc_type {
            return Some(ds_core::DocumentType { doc_type, confidence: 1.0, source: DocTypeSource::Caller });
        }
        let classifier = self.doc_classifier.as_ref()?;
        let n = self.doc_classify_pages.clamp(1, 8).min(page_count as usize);
        let mut thumbnails = Vec::with_capacity(n);
        for page_no in 1..=n as u32 {
            let key = keys::thumbnail(document_hk.as_str(), page_no);
            match self.blob.get(&key).await {
                Ok(bytes) => {
                    let (width_px, height_px) = match crate::page::image_dimensions(&bytes) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!(%document_hk, page_no, error = %e, "unreadable thumbnail; not classifying the document");
                            return None;
                        }
                    };
                    thumbnails.push(PageImage { uri: key, bytes: Arc::from(bytes.as_ref()), width_px, height_px, dpi: 0 });
                }
                Err(e) => {
                    tracing::warn!(%document_hk, page_no, error = %e, "thumbnail unavailable; not classifying the document");
                    return None;
                }
            }
        }
        match classifier.classify(&thumbnails, &spec.language).await {
            Ok(dt) => {
                tracing::info!(%document_hk, doc_type = ?dt.doc_type, confidence = dt.confidence, pages = thumbnails.len(), "document type settled before routing");
                metrics::counter!("worker_document_type_total", "source" => "classifier").increment(1);
                Some(dt)
            }
            Err(e) => {
                // Every page still classifies itself; this only loses the smoothing.
                tracing::warn!(%document_hk, error = %e, "document classification failed; pages will route on their own class");
                metrics::counter!("worker_document_type_total", "source" => "failed").increment(1);
                None
            }
        }
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
