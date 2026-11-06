use std::io::Cursor;
use std::sync::Arc;

use image::{DynamicImage, ImageFormat};
use vs_classify::PageSignals;
use vs_dag::{PageImage, TextLayer};

use crate::format::Format;

pub use crate::image_raster::ImageRasterizer;
pub use crate::pdf::PdfiumRasterizer;

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
    #[error("pdfium unavailable: {0}")]
    Pdfium(String),
    #[error("i/o: {0}")]
    Io(String),
    #[error("render timed out on page {page}")]
    Timeout { page: u32 },
    #[error("renderer crashed on page {page}: {reason}")]
    Crashed { page: u32, reason: String },
}

impl IngestError {
    /// Deterministic errors are not retried. A hang or repeated crash is a property of
    /// the input, so it is permanent too.
    pub fn is_permanent(&self) -> bool {
        !matches!(self, IngestError::Render { .. } | IngestError::Pdfium(_) | IngestError::Io(_))
    }
}

/// Everything ingest stores for one page.
#[derive(Debug, Clone)]
pub struct PageAssets {
    pub image: PageImage,
    pub thumbnail: PageImage,
    pub text_layer: TextLayer,
    pub signals: PageSignals,
}

/// One opened document. Page numbers are 1-based. Implementations are not required
/// to be `Sync`; callers render on blocking threads.
pub trait Rasterizer: Send {
    fn page_count(&self) -> u32;
    fn render(&mut self, page_no: u32, dpi: u32) -> Result<PageImage, IngestError>;
    fn thumbnail(&mut self, page_no: u32, long_side_px: u32) -> Result<PageImage, IngestError>;
    fn text_layer(&mut self, page_no: u32) -> Result<TextLayer, IngestError>;
    fn signals(&mut self, page_no: u32) -> Result<PageSignals, IngestError>;

    /// All of a page's assets. Implementations override this when doing it in one pass
    /// is cheaper than four calls.
    fn page(&mut self, page_no: u32, dpi: u32, thumb_long_side_px: u32) -> Result<PageAssets, IngestError> {
        Ok(PageAssets {
            image: self.render(page_no, dpi)?,
            thumbnail: self.thumbnail(page_no, thumb_long_side_px)?,
            text_layer: self.text_layer(page_no)?,
            signals: self.signals(page_no)?,
        })
    }
}

pub fn open_rasterizer(format: Format, bytes: Vec<u8>, max_pages: u32) -> Result<Box<dyn Rasterizer>, IngestError> {
    match format {
        Format::Pdf => Ok(Box::new(PdfiumRasterizer::open(bytes, max_pages)?)),
        Format::Png | Format::Jpeg | Format::Tiff => Ok(Box::new(ImageRasterizer::open(format, &bytes, max_pages)?)),
    }
}

/// Opens a document from a local file. PDFs are read lazily by pdfium; images are read
/// into memory (one page at a time is decoded).
pub fn open_rasterizer_file(format: Format, path: &std::path::Path, max_pages: u32) -> Result<Box<dyn Rasterizer>, IngestError> {
    match format {
        Format::Pdf => Ok(Box::new(PdfiumRasterizer::open_file(path, max_pages)?)),
        Format::Png | Format::Jpeg | Format::Tiff => {
            let bytes = std::fs::read(path).map_err(|e| IngestError::Io(e.to_string()))?;
            Ok(Box::new(ImageRasterizer::open(format, &bytes, max_pages)?))
        }
    }
}

pub(crate) fn check_page(page_no: u32, page_count: u32) -> Result<(), IngestError> {
    if page_no == 0 || page_no > page_count {
        return Err(IngestError::Render { page: page_no, reason: format!("page out of range 1..={page_count}") });
    }
    Ok(())
}

/// Page image as PNG (RGB, lossless; the model input).
pub(crate) fn encode_png(img: &DynamicImage, dpi: u32, page: u32) -> Result<PageImage, IngestError> {
    let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
    let mut out = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|e| IngestError::Render { page, reason: e.to_string() })?;
    Ok(page_image(out, &rgb, dpi))
}

/// Classifier thumbnail as JPEG q=85.
pub(crate) fn encode_jpeg(img: &DynamicImage, dpi: u32, page: u32) -> Result<PageImage, IngestError> {
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
        .encode_image(&rgb)
        .map_err(|e| IngestError::Render { page, reason: e.to_string() })?;
    Ok(page_image(out, img, dpi))
}

fn page_image(bytes: Vec<u8>, img: &DynamicImage, dpi: u32) -> PageImage {
    // `uri` is set by the caller once the image is stored.
    PageImage { uri: String::new(), bytes: Arc::from(bytes), width_px: img.width(), height_px: img.height(), dpi }
}
