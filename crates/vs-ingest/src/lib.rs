//! Ingest: format detection, rasterization, text layer, CPU steps. See `SPEC.md`.

pub mod format;
pub mod raster;
pub mod steps;

pub use format::{detect_format, Format};
pub use raster::{IngestError, PdfiumRasterizer, Rasterizer};
