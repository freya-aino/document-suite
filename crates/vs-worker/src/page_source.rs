//! Re-rendering a page at a finer DPI, for `crop_dpi`.
//!
//! The problem this solves: a page is rasterised once, at `worker.render_dpi`, and every
//! crop is cut from that bitmap. A figure occupying a tenth of the page is then described
//! from a tenth of the page's pixels, which for a dense chart or a small formula is the
//! difference between reading it and guessing. `crop_dpi` was documented for exactly that
//! and could not be implemented: a step had no way back to the original document, because
//! `StepContext` carried no store and giving every step one would have made the store the
//! step interface.
//!
//! What a step gets instead is a *capability* — [`vs_dag::PageSource`] — and this is the
//! worker's implementation of it: fetch the original, render the one page asked for, keep
//! the result in the object store under its own DPI so the next ask is a download.
//!
//! The costs, stated because a DAG author is choosing to pay them: one download of the
//! original and one render per (page, DPI) that is not already stored. Both are per
//! document rather than per crop — every region on a page shares the render — and the
//! object is cached, so a re-run of the document pays nothing.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use vs_core::HashKey;
use vs_dag::{PageImage, PageSource, PageSourceError};
use vs_infra::blob::{key_from_uri, keys, BlobStore};
use vs_ingest::{detect_format, Format, RenderPool};

/// Renders pages of one document on demand.
///
/// Built per page task, so it holds that document's identity and nothing else: a step
/// asks for "page 3 at 400 dpi" and cannot ask about another document.
pub struct RenderOnDemand {
    pub blob: Arc<dyn BlobStore>,
    pub pool: Arc<RenderPool>,
    pub bucket: String,
    pub document_hk: HashKey,
    /// Where the original is, from the job spec.
    pub source_uri: String,
    pub scratch_dir: PathBuf,
    /// Pages beyond this are refused, as in ingest.
    pub max_pages: u32,
    /// Finest DPI a step may ask for.
    ///
    /// A render is quadratic in DPI: a page at 1200 dpi is nine times the pixels of one at
    /// 400, and a step asking for it per region would quietly turn a page into minutes of
    /// CPU and a gigabyte of bitmap. The cap is what a DAG author's typo runs into.
    pub max_dpi: u32,
}

/// Lower bound on what a re-render is worth doing for. Below this, the page image the
/// step already has is at least as good.
const MIN_DPI: u32 = 72;

#[async_trait::async_trait]
impl PageSource for RenderOnDemand {
    async fn page_at_dpi(&self, page_no: u32, dpi: u32) -> Result<PageImage, PageSourceError> {
        let dpi = dpi.clamp(MIN_DPI, self.max_dpi);
        let key = keys::page_image(self.document_hk.as_str(), page_no, dpi);

        // Already rendered — by an earlier node, an earlier run, or ingest itself if the
        // page DPI happens to match.
        if let Ok(bytes) = self.blob.get(&key).await {
            return image(key, bytes, dpi);
        }

        let spool = Spool(self.scratch_dir.join(format!("{}-{page_no}-{dpi}.src", self.document_hk)));
        self.download(&spool.0).await?;
        let format = sniff(&spool.0)
            .await
            .map_err(|e| PageSourceError::Unavailable(format!("cannot read the spooled original: {e}")))?
            .ok_or_else(|| PageSourceError::Unavailable("the original is not in a format this suite renders".into()))?;

        let assets = self
            .pool
            // The thumbnail is rendered too and thrown away: the pool's protocol produces
            // both, and a second call to avoid one small JPEG would cost another render
            // of the page.
            .page(&spool.0, format, self.max_pages, page_no, dpi, 1)
            .await
            .map_err(|e| PageSourceError::Render { page_no, dpi, reason: e.to_string() })?;

        let bytes = Bytes::from(assets.image.bytes.to_vec());
        // Stored under its own DPI, so the next step, the next page of the same document
        // and the next run all find it. Failure to store is not failure to render.
        if let Err(e) = self.blob.put(&key, bytes.clone(), "image/png").await {
            tracing::warn!(key = %key, error = %e, "could not cache a re-rendered page");
        }
        tracing::info!(document_hk = %self.document_hk, page_no, dpi, format = ?format, "re-rendered a page at a finer dpi");
        metrics::counter!("worker_page_rerenders_total").increment(1);
        image(key, bytes, dpi)
    }
}

impl RenderOnDemand {
    async fn download(&self, path: &std::path::Path) -> Result<(), PageSourceError> {
        let key = key_from_uri(&self.source_uri, &self.bucket);
        let mut stream = self
            .blob
            .get_stream(key)
            .await
            // The original may be past its retention (`blob.retention.originals_days`),
            // which is a legitimate state and not an error in this code: a step that
            // cannot get a finer render falls back to the page image it has.
            .map_err(|e| PageSourceError::Unavailable(format!("{key}: {e}")))?;
        let mut file = tokio::fs::File::create(path).await.map_err(|e| PageSourceError::Unavailable(e.to_string()))?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| PageSourceError::Unavailable(e.to_string()))?;
            file.write_all(&chunk).await.map_err(|e| PageSourceError::Unavailable(e.to_string()))?;
        }
        file.flush().await.map_err(|e| PageSourceError::Unavailable(e.to_string()))?;
        Ok(())
    }
}

fn image(uri: String, bytes: Bytes, dpi: u32) -> Result<PageImage, PageSourceError> {
    let (width_px, height_px) =
        crate::page::image_dimensions(&bytes).map_err(|e| PageSourceError::Render { page_no: 0, dpi, reason: e.to_string() })?;
    Ok(PageImage { uri, bytes: Arc::from(bytes.as_ref()), width_px, height_px, dpi })
}

/// Deletes the spooled original when the render ends, however it ends.
struct Spool(PathBuf);

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn sniff(path: &std::path::Path) -> std::io::Result<Option<Format>> {
    let mut head = Vec::with_capacity(1024);
    tokio::fs::File::open(path).await?.take(1024).read_to_end(&mut head).await?;
    Ok(detect_format(&head))
}
