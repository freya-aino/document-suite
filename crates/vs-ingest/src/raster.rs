use vs_classify::PageSignals;
use vs_dag::{PageImage, TextLayer};

use crate::format::Format;

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("unsupported format")]
    UnsupportedFormat,
    #[error("document is encrypted")]
    Encrypted,
    #[error("corrupt document: {0}")]
    Corrupt(String),
    #[error("too many pages: {0}")]
    TooManyPages(u32),
    #[error("render failed on page {page}: {reason}")]
    Render { page: u32, reason: String },
}

impl IngestError {
    /// Deterministic errors are not retried.
    pub fn is_permanent(&self) -> bool {
        !matches!(self, IngestError::Render { .. })
    }
}

/// One opened document. Implementations are not required to be `Sync`;
/// callers render on blocking threads.
pub trait Rasterizer: Send {
    fn page_count(&self) -> u32;
    fn render(&mut self, page_no: u32, dpi: u32) -> Result<PageImage, IngestError>;
    fn thumbnail(&mut self, page_no: u32, long_side_px: u32) -> Result<PageImage, IngestError>;
    fn text_layer(&mut self, page_no: u32) -> Result<TextLayer, IngestError>;
    fn signals(&mut self, page_no: u32) -> Result<PageSignals, IngestError>;
}

/// pdfium-backed rasterizer for PDFs (`libpdfium.so` in the rootfs).
pub struct PdfiumRasterizer {
    _bytes: Vec<u8>,
    page_count: u32,
}

impl PdfiumRasterizer {
    pub fn open(bytes: Vec<u8>, max_pages: u32) -> Result<Self, IngestError> {
        let _ = max_pages;
        let _ = bytes;
        todo!("bind pdfium (Pdfium::bind_to_system_library), load document, check page count")
    }
}

impl Rasterizer for PdfiumRasterizer {
    fn page_count(&self) -> u32 {
        self.page_count
    }
    fn render(&mut self, _page_no: u32, _dpi: u32) -> Result<PageImage, IngestError> {
        todo!("render page to RGB bitmap, encode PNG")
    }
    fn thumbnail(&mut self, _page_no: u32, _long_side_px: u32) -> Result<PageImage, IngestError> {
        todo!("render at target size, encode JPEG q=85")
    }
    fn text_layer(&mut self, _page_no: u32) -> Result<TextLayer, IngestError> {
        todo!("iterate text segments; normalise bboxes to [0,1]")
    }
    fn signals(&mut self, _page_no: u32) -> Result<PageSignals, IngestError> {
        todo!("count chars, image objects/areas, detect invisible text render mode (OCR layer), rotation")
    }
}

/// Image formats (PNG/JPEG/TIFF frames) via the `image` crate.
pub fn open_rasterizer(format: Format, bytes: Vec<u8>, max_pages: u32) -> Result<Box<dyn Rasterizer>, IngestError> {
    match format {
        Format::Pdf => Ok(Box::new(PdfiumRasterizer::open(bytes, max_pages)?)),
        Format::Png | Format::Jpeg | Format::Tiff => todo!("ImageRasterizer: decode frames, apply EXIF orientation"),
    }
}
