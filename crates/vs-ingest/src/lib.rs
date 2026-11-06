//! Ingest: format detection, rasterization, text layer, CPU steps. See `SPEC.md`.

pub mod child;
pub mod format;
mod image_raster;
mod pdf;
pub mod pool;
pub mod raster;
pub mod steps;

pub use format::{detect_format, Format};
pub use pdf::{pdfium, PdfiumGuard};
pub use pool::{ChildCommand, PoolConfig, RenderPool};
pub use raster::{open_rasterizer, open_rasterizer_file, ImageRasterizer, IngestError, PageAssets, PdfiumRasterizer, Rasterizer};
