//! Ingest: format detection, rasterization, text layer, CPU steps. See `SPEC.md`.

pub mod format;
mod image_raster;
mod pdf;
pub mod raster;
pub mod steps;

pub use format::{detect_format, Format};
pub use pdf::{pdfium, PdfiumGuard};
pub use raster::{open_rasterizer, ImageRasterizer, IngestError, PageAssets, PdfiumRasterizer, Rasterizer};
