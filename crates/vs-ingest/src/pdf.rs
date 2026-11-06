//! PDF rasterizer on pdfium (`pdfium-render`, dynamic binding).

use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, OnceLock};

use image::DynamicImage;
use pdfium_render::prelude::*;
use vs_classify::PageSignals;
use vs_core::BBox;
use vs_dag::{PageImage, TextLayer, TextSpan};

use crate::raster::{check_page, encode_jpeg, encode_png, IngestError, PageAssets, Rasterizer};

/// Largest rendered side in pixels; guards against huge page boxes at high DPI.
const MAX_RENDER_PX: i32 = 12_000;
/// Image objects covering at least this share of the page count as "full page".
const FULL_PAGE_IMAGE: f32 = 0.8;

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();
static PDFIUM_LOCK: Mutex<()> = Mutex::new(());

/// Exclusive use of the process-wide pdfium binding; dereferences to [`Pdfium`].
///
/// pdfium is not thread-safe and keeps global state. `pdfium-render`'s own locking is
/// per FFI call, which is not enough once documents are open on several threads
/// (heap corruption in tests), so every document operation holds this guard from open
/// to close. Rendering parallelism therefore comes from more worker processes, not
/// threads.
pub struct PdfiumGuard {
    pdfium: &'static Pdfium,
    _lock: MutexGuard<'static, ()>,
}

impl Deref for PdfiumGuard {
    type Target = Pdfium;
    fn deref(&self) -> &Pdfium {
        self.pdfium
    }
}

/// Binds pdfium on first use: `PDFIUM_DYNAMIC_LIB_PATH` first, then the system library
/// path. Blocks while another thread holds the guard.
pub fn pdfium() -> Result<PdfiumGuard, IngestError> {
    let lock = PDFIUM_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let pdfium = PDFIUM
        .get_or_init(|| {
            let bindings = match std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH") {
                Some(dir) => Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)),
                None => Pdfium::bind_to_system_library(),
            };
            bindings.map(Pdfium::new).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| IngestError::Pdfium(e.clone()))?;
    Ok(PdfiumGuard { pdfium, _lock: lock })
}

/// One PDF. Holds the bytes and reopens the document per call: `PdfDocument` borrows
/// both the bytes and the binding, and `page()` does a page's work in a single open.
pub struct PdfiumRasterizer {
    bytes: Vec<u8>,
    page_count: u32,
}

impl PdfiumRasterizer {
    pub fn open(bytes: Vec<u8>, max_pages: u32) -> Result<Self, IngestError> {
        let page_count = {
            let pdfium = pdfium()?;
            let doc = pdfium.load_pdf_from_byte_slice(&bytes, None).map_err(load_error)?;
            doc.pages().len() as u32
        };
        if page_count > max_pages {
            return Err(IngestError::TooManyPages(page_count));
        }
        Ok(Self { bytes, page_count })
    }

    fn with_page<T>(&self, page_no: u32, f: impl FnOnce(&PdfPage, &PageGeom) -> Result<T, IngestError>) -> Result<T, IngestError> {
        check_page(page_no, self.page_count)?;
        let render = |e: PdfiumError| IngestError::Render { page: page_no, reason: e.to_string() };
        let pdfium = pdfium()?;
        let doc = pdfium.load_pdf_from_byte_slice(&self.bytes, None).map_err(render)?;
        let page = doc.pages().get((page_no - 1) as PdfPageIndex).map_err(render)?;
        let geom = PageGeom::of(&page);
        f(&page, &geom)
    }
}

// Bitmaps are rendered under the pdfium guard; PNG/JPEG encoding happens after it is
// released so other threads can render meanwhile.
impl Rasterizer for PdfiumRasterizer {
    fn page_count(&self) -> u32 {
        self.page_count
    }

    fn render(&mut self, page_no: u32, dpi: u32) -> Result<PageImage, IngestError> {
        let (img, dpi) = self.with_page(page_no, |p, g| render(p, g, page_no, dpi as f32 / 72.0))?;
        encode_png(&img, dpi, page_no)
    }

    fn thumbnail(&mut self, page_no: u32, long_side_px: u32) -> Result<PageImage, IngestError> {
        let (img, dpi) = self.with_page(page_no, |p, g| thumbnail(p, g, page_no, long_side_px))?;
        encode_jpeg(&img, dpi, page_no)
    }

    fn text_layer(&mut self, page_no: u32) -> Result<TextLayer, IngestError> {
        self.with_page(page_no, |p, g| text_layer(p, g, page_no))
    }

    fn signals(&mut self, page_no: u32) -> Result<PageSignals, IngestError> {
        self.with_page(page_no, |p, g| {
            let layer = text_layer(p, g, page_no)?;
            signals(p, g, &layer, page_no)
        })
    }

    fn page(&mut self, page_no: u32, dpi: u32, thumb_long_side_px: u32) -> Result<PageAssets, IngestError> {
        let (image, thumb, text_layer, signals) = self.with_page(page_no, |p, g| {
            let layer = text_layer(p, g, page_no)?;
            let signals = signals(p, g, &layer, page_no)?;
            Ok((render(p, g, page_no, dpi as f32 / 72.0)?, thumbnail(p, g, page_no, thumb_long_side_px)?, layer, signals))
        })?;
        Ok(PageAssets {
            image: encode_png(&image.0, image.1, page_no)?,
            thumbnail: encode_jpeg(&thumb.0, thumb.1, page_no)?,
            text_layer,
            signals,
        })
    }
}

fn load_error(e: PdfiumError) -> IngestError {
    match e {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => IngestError::Encrypted,
        other => IngestError::Corrupt(other.to_string()),
    }
}

/// Maps PDF user space (origin bottom-left, unrotated, points) to normalised page
/// coordinates of the page as displayed (origin top-left, `/Rotate` applied), which is
/// how pdfium renders it.
struct PageGeom {
    /// Displayed size in points (pdfium reports page size with rotation applied).
    width: f32,
    height: f32,
    rotation: u16,
}

impl PageGeom {
    fn of(page: &PdfPage) -> Self {
        let rotation = match page.rotation() {
            Ok(PdfPageRenderRotation::Degrees90) => 90,
            Ok(PdfPageRenderRotation::Degrees180) => 180,
            Ok(PdfPageRenderRotation::Degrees270) => 270,
            _ => 0,
        };
        Self { width: page.width().value, height: page.height().value, rotation }
    }

    /// Unrotated page size in points.
    fn unrotated(&self) -> (f32, f32) {
        if self.rotation % 180 == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    fn bbox(&self, rect: &PdfRect) -> Option<BBox> {
        let (w, h) = self.unrotated();
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let n = |v: f32| v.clamp(0.0, 1.0);
        let (u0, u1) = (n(rect.left().value / w), n(rect.right().value / w));
        let (v0, v1) = (n(1.0 - rect.top().value / h), n(1.0 - rect.bottom().value / h));
        // Rotate clockwise by /Rotate: 90° maps (u, v) → (1 - v, u).
        let rot = |u: f32, v: f32| match self.rotation {
            90 => (1.0 - v, u),
            180 => (1.0 - u, 1.0 - v),
            270 => (v, 1.0 - u),
            _ => (u, v),
        };
        let (a, b) = (rot(u0, v0), rot(u1, v1));
        let bb = BBox::new(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)).ok()?;
        (bb.area() > 0.0).then_some(bb)
    }
}

/// Rendered bitmap and its effective DPI (after the size cap).
fn render(page: &PdfPage, geom: &PageGeom, page_no: u32, scale: f32) -> Result<(DynamicImage, u32), IngestError> {
    let config = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .set_maximum_width(MAX_RENDER_PX)
        .set_maximum_height(MAX_RENDER_PX)
        .render_form_data(true);
    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| IngestError::Render { page: page_no, reason: e.to_string() })?;
    let img = bitmap.as_image();
    let dpi = (img.width() as f32 * 72.0 / geom.width.max(1.0)).round() as u32;
    Ok((img, dpi))
}

fn thumbnail(page: &PdfPage, geom: &PageGeom, page_no: u32, long_side_px: u32) -> Result<(DynamicImage, u32), IngestError> {
    let scale = long_side_px as f32 / geom.width.max(geom.height).max(1.0);
    let config = PdfRenderConfig::new().scale_page_by_factor(scale).render_form_data(true);
    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| IngestError::Render { page: page_no, reason: e.to_string() })?;
    Ok((bitmap.as_image(), (72.0 * scale).round() as u32))
}

fn text_layer(page: &PdfPage, geom: &PageGeom, page_no: u32) -> Result<TextLayer, IngestError> {
    let text = page.text().map_err(|e| IngestError::Render { page: page_no, reason: e.to_string() })?;
    let spans = text
        .segments()
        .iter()
        .filter_map(|seg| {
            let s = seg.text();
            if s.trim().is_empty() {
                return None;
            }
            let bbox = geom.bbox(&seg.bounds())?;
            let font_size = seg.chars().ok().and_then(|cs| cs.iter().next().map(|c| c.scaled_font_size().value));
            Some(TextSpan { text: s, bbox, font_size })
        })
        .collect();
    Ok(TextLayer { spans })
}

/// Classifier signals (`vs-classify` tier 0). Coverage follows the v1 approximation in
/// SPEC.md: span area ÷ (span area + area of images that are full-page or hold no text).
fn signals(page: &PdfPage, geom: &PageGeom, layer: &TextLayer, page_no: u32) -> Result<PageSignals, IngestError> {
    let text = page.text().map_err(|e| IngestError::Render { page: page_no, reason: e.to_string() })?;
    let text_chars = text.all().chars().filter(|c| !c.is_whitespace()).count() as u32;

    let mut images = Vec::new();
    let mut invisible_text = false;
    for obj in page.objects().iter() {
        visit(&obj, geom, &mut images, &mut invisible_text);
    }

    let span_area: f32 = layer.spans.iter().map(|s| s.bbox.area()).sum::<f32>().min(1.0);
    let has_span_inside = |img: &BBox| {
        layer.spans.iter().any(|s| {
            let (cx, cy) = ((s.bbox.x0 + s.bbox.x1) / 2.0, (s.bbox.y0 + s.bbox.y1) / 2.0);
            cx >= img.x0 && cx <= img.x1 && cy >= img.y0 && cy <= img.y1
        })
    };
    let image_text_area: f32 = images
        .iter()
        .filter(|b| b.area() >= FULL_PAGE_IMAGE || !has_span_inside(b))
        .map(|b| b.area())
        .sum::<f32>()
        .min(1.0);
    let text_layer_coverage = if span_area + image_text_area > 0.0 { span_area / (span_area + image_text_area) } else { 0.0 };

    Ok(PageSignals {
        text_chars,
        text_layer_coverage,
        ocr_text_layer: invisible_text && images.iter().any(|b| b.area() >= FULL_PAGE_IMAGE),
        image_count: images.len() as u32,
        image_area_ratio: images.iter().map(|b| b.area()).sum::<f32>().min(1.0),
        rotation_deg: geom.rotation,
    })
}

/// Collects image boxes and whether invisible text is present, descending into form
/// XObjects (scanners often wrap the page image in one).
fn visit(obj: &PdfPageObject, geom: &PageGeom, images: &mut Vec<BBox>, invisible_text: &mut bool) {
    match obj.object_type() {
        PdfPageObjectType::Image => {
            if let Some(b) = obj.bounds().ok().and_then(|q| geom.bbox(&q.to_rect())) {
                images.push(b);
            }
        }
        PdfPageObjectType::Text => {
            if let Some(t) = obj.as_text_object() {
                *invisible_text |= t.render_mode() == PdfPageTextRenderMode::Invisible;
            }
        }
        PdfPageObjectType::XObjectForm => {
            if let Some(form) = obj.as_x_object_form_object() {
                for child in form.iter() {
                    visit(&child, geom, images, invisible_text);
                }
            }
        }
        _ => {}
    }
}
