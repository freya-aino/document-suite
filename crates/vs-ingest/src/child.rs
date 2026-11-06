//! Render child process: renders pages for a [`crate::pool::RenderPool`] so that a PDF
//! which hangs or crashes pdfium takes down only the child, never the worker.
//!
//! Protocol (stateless; every request names the file, so any child can serve any page
//! and a child can be killed at any time):
//! - request: one JSON line on stdin ([`Request`]);
//! - reply on stdout: `u32` big-endian length + JSON [`Reply`], then for a page reply
//!   two more length-prefixed payloads: the PNG page image and the JPEG thumbnail.
//!
//! The child writes nothing else to stdout; logs go to stderr.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vs_classify::PageSignals;
use vs_dag::{PageImage, TextLayer};

use crate::format::Format;
use crate::raster::{open_rasterizer_file, IngestError, PageAssets};

/// Address-space limit a child sets on itself (`VS_RENDER_MEM_LIMIT_MB`).
pub const MEM_LIMIT_ENV: &str = "VS_RENDER_MEM_LIMIT_MB";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Count { path: PathBuf, format: Format, max_pages: u32 },
    Page { path: PathBuf, format: Format, max_pages: u32, page_no: u32, dpi: u32, thumb_long_side_px: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Count { page_count: u32 },
    /// Followed by the PNG and JPEG payloads.
    Page { image: ImageMeta, thumbnail: ImageMeta, text_layer: TextLayer, signals: PageSignals },
    Error { error: WireError },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ImageMeta {
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: u32,
}

/// `IngestError` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireError {
    UnsupportedFormat,
    Encrypted,
    Corrupt { reason: String },
    TooManyPages { pages: u32 },
    Render { page: u32, reason: String },
    Pdfium { reason: String },
    Io { reason: String },
}

impl From<&IngestError> for WireError {
    fn from(e: &IngestError) -> Self {
        match e {
            IngestError::UnsupportedFormat => WireError::UnsupportedFormat,
            IngestError::Encrypted => WireError::Encrypted,
            IngestError::Corrupt(r) => WireError::Corrupt { reason: r.clone() },
            IngestError::TooManyPages(n) => WireError::TooManyPages { pages: *n },
            IngestError::Render { page, reason } => WireError::Render { page: *page, reason: reason.clone() },
            IngestError::Pdfium(r) => WireError::Pdfium { reason: r.clone() },
            // Timeouts and crashes are decided by the parent; a child never reports them.
            other => WireError::Io { reason: other.to_string() },
        }
    }
}

impl From<WireError> for IngestError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::UnsupportedFormat => IngestError::UnsupportedFormat,
            WireError::Encrypted => IngestError::Encrypted,
            WireError::Corrupt { reason } => IngestError::Corrupt(reason),
            WireError::TooManyPages { pages } => IngestError::TooManyPages(pages),
            WireError::Render { page, reason } => IngestError::Render { page, reason },
            WireError::Pdfium { reason } => IngestError::Pdfium(reason),
            WireError::Io { reason } => IngestError::Io(reason),
        }
    }
}

/// Child main loop: serve requests until stdin closes.
pub fn serve(input: impl Read, output: impl Write) -> io::Result<()> {
    die_with_parent();
    limit_memory_from_env();
    let mut out = io::BufWriter::new(output);
    for line in io::BufReader::new(input).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write_reply(&mut out, &Reply::Error { error: WireError::Io { reason: format!("bad request: {e}") } }, &[])?;
                continue;
            }
        };
        match handle(req) {
            Ok((reply, payloads)) => write_reply(&mut out, &reply, &payloads)?,
            Err(e) => write_reply(&mut out, &Reply::Error { error: WireError::from(&e) }, &[])?,
        }
    }
    Ok(())
}

fn handle(req: Request) -> Result<(Reply, Vec<Arc<[u8]>>), IngestError> {
    match req {
        Request::Count { path, format, max_pages } => {
            let r = open_rasterizer_file(format, &path, max_pages)?;
            Ok((Reply::Count { page_count: r.page_count() }, vec![]))
        }
        Request::Page { path, format, max_pages, page_no, dpi, thumb_long_side_px } => {
            let mut r = open_rasterizer_file(format, &path, max_pages)?;
            let a = r.page(page_no, dpi, thumb_long_side_px)?;
            let meta = |i: &PageImage| ImageMeta { width_px: i.width_px, height_px: i.height_px, dpi: i.dpi };
            let reply = Reply::Page { image: meta(&a.image), thumbnail: meta(&a.thumbnail), text_layer: a.text_layer, signals: a.signals };
            Ok((reply, vec![a.image.bytes, a.thumbnail.bytes]))
        }
    }
}

fn write_reply(out: &mut impl Write, reply: &Reply, payloads: &[Arc<[u8]>]) -> io::Result<()> {
    let header = serde_json::to_vec(reply).map_err(io::Error::other)?;
    write_frame(out, &header)?;
    for p in payloads {
        write_frame(out, p)?;
    }
    out.flush()
}

pub(crate) fn write_frame(out: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let len = u32::try_from(data.len()).map_err(|_| io::Error::other("frame over 4 GiB"))?;
    out.write_all(&len.to_be_bytes())?;
    out.write_all(data)
}

/// Rebuilds `PageAssets` from a page reply and its two payloads.
pub(crate) fn page_assets(image: ImageMeta, thumb: ImageMeta, text_layer: TextLayer, signals: PageSignals, png: Vec<u8>, jpeg: Vec<u8>) -> PageAssets {
    let img = |m: ImageMeta, bytes: Vec<u8>| PageImage { uri: String::new(), bytes: Arc::from(bytes), width_px: m.width_px, height_px: m.height_px, dpi: m.dpi };
    PageAssets { image: img(image, png), thumbnail: img(thumb, jpeg), text_layer, signals }
}

/// An idle child exits when its stdin closes; a child stuck in pdfium would not, so ask
/// the kernel to kill it when the worker dies (even by SIGKILL).
fn die_with_parent() {
    #[cfg(target_os = "linux")]
    // SAFETY: prctl(PR_SET_PDEATHSIG) with a signal number; affects only this process.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
}

fn limit_memory_from_env() {
    let Some(mb) = std::env::var(MEM_LIMIT_ENV).ok().and_then(|v| v.parse::<u64>().ok()) else { return };
    let bytes = mb.saturating_mul(1024 * 1024);
    let lim = libc::rlimit { rlim_cur: bytes as libc::rlim_t, rlim_max: bytes as libc::rlim_t };
    // SAFETY: setrlimit with a valid, fully initialised struct; affects only this process.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &lim) } != 0 {
        eprintln!("render child: setrlimit failed: {}", io::Error::last_os_error());
    }
}
