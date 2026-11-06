//! Rasterizer tests on generated fixtures. PDF tests need libpdfium
//! (`PDFIUM_DYNAMIC_LIB_PATH`, set by devenv); they are skipped without it.

use std::io::Cursor;

use image::{DynamicImage, GrayImage};
use pdfium_render::prelude::*;
use vs_ingest::{detect_format, open_rasterizer, pdfium, Format, IngestError, PdfiumGuard};

const DPI: u32 = 200;
const THUMB: u32 = 768;

fn pdfium_or_skip() -> Option<PdfiumGuard> {
    match pdfium() {
        Ok(p) => Some(p),
        Err(e) if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none() => {
            eprintln!("skipping: {e}");
            None
        }
        Err(e) => panic!("PDFIUM_DYNAMIC_LIB_PATH is set but binding failed: {e}"),
    }
}

/// A4 page; text at (72, 750) pt from the bottom-left; optional full-page scan image
/// with the text made invisible (a pre-OCR'd scan); optional /Rotate.
fn make_pdf(p: PdfiumGuard, scan: bool, rotation: PdfPageRenderRotation) -> Vec<u8> {
    let mut doc = p.create_new_pdf().unwrap();
    let font = doc.fonts_mut().helvetica();
    let mut page = doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
    if scan {
        let img = DynamicImage::ImageLuma8(GrayImage::from_pixel(200, 283, image::Luma([230])));
        let (w, h) = (page.width(), page.height());
        page.objects_mut()
            .create_image_object(PdfPoints::ZERO, PdfPoints::ZERO, &img, Some(w), Some(h))
            .unwrap();
    }
    // Render mode must be set before the object is added to the page to be saved.
    let mut text = PdfPageTextObject::new(&doc, "Hello visual suite", font, PdfPoints::new(12.0)).unwrap();
    text.translate(PdfPoints::new(72.0), PdfPoints::new(750.0)).unwrap();
    if scan {
        text.set_render_mode(PdfPageTextRenderMode::Invisible).unwrap();
    }
    page.objects_mut().add_text_object(text).unwrap();
    page.set_rotation(rotation);
    drop(page);
    doc.save_to_bytes().unwrap()
}

#[test]
fn born_digital_page() {
    let Some(p) = pdfium_or_skip() else { return };
    let bytes = make_pdf(p, false, PdfPageRenderRotation::None);
    assert_eq!(detect_format(&bytes), Some(Format::Pdf));
    let mut r = open_rasterizer(Format::Pdf, bytes, 10).unwrap();
    assert_eq!(r.page_count(), 1);

    let a = r.page(1, DPI, THUMB).unwrap();
    // A4 = 595 x 842 pt
    assert!((a.image.width_px as i32 - 1653).abs() <= 2, "width {}", a.image.width_px);
    assert!((a.image.height_px as i32 - 2339).abs() <= 2, "height {}", a.image.height_px);
    assert_eq!(a.image.dpi, DPI);
    assert_eq!(a.thumbnail.height_px, THUMB);
    assert_eq!(&a.image.bytes[1..4], b"PNG");
    assert_eq!(&a.thumbnail.bytes[..2], &[0xFF, 0xD8]);

    let span = a.text_layer.spans.iter().find(|s| s.text.contains("Hello")).expect("text span");
    assert!((span.bbox.x0 - 72.0 / 595.0).abs() < 0.01, "{:?}", span.bbox);
    assert!(span.bbox.y1 < 0.12 && span.bbox.y0 > 0.08, "{:?}", span.bbox);
    assert!((span.font_size.unwrap() - 12.0).abs() < 0.5);

    let s = a.signals;
    assert_eq!(s.text_chars, "Hellovisualsuite".len() as u32);
    assert_eq!(s.image_count, 0);
    assert!(!s.ocr_text_layer);
    assert!((s.text_layer_coverage - 1.0).abs() < 1e-6);
}

#[test]
fn scanned_page_with_ocr_layer() {
    let Some(p) = pdfium_or_skip() else { return };
    let mut r = open_rasterizer(Format::Pdf, make_pdf(p, true, PdfPageRenderRotation::None), 10).unwrap();
    let s = r.signals(1).unwrap();
    assert_eq!(s.image_count, 1);
    assert!(s.image_area_ratio > 0.99);
    assert!(s.ocr_text_layer);
    assert!(s.text_layer_coverage < 0.1, "coverage {}", s.text_layer_coverage);
}

#[test]
fn rotated_page_boxes_follow_rendering() {
    let Some(p) = pdfium_or_skip() else { return };
    let mut r = open_rasterizer(Format::Pdf, make_pdf(p, false, PdfPageRenderRotation::Degrees90), 10).unwrap();
    let a = r.page(1, 72, THUMB).unwrap();
    assert!(a.image.width_px > a.image.height_px, "rendered landscape");
    assert_eq!(a.signals.rotation_deg, 90);
    // Text near the unrotated top-left ends up near the top-right after 90° clockwise.
    let b = a.text_layer.spans[0].bbox;
    assert!(b.x0 > 0.85 && b.y0 < 0.3, "{b:?}");
}

#[test]
fn page_out_of_range_and_garbage() {
    let Some(p) = pdfium_or_skip() else { return };
    let bytes = make_pdf(p, false, PdfPageRenderRotation::None);
    let mut r = open_rasterizer(Format::Pdf, bytes.clone(), 10).unwrap();
    assert!(matches!(r.render(2, DPI), Err(IngestError::Render { page: 2, .. })));
    assert!(matches!(r.render(0, DPI), Err(IngestError::Render { page: 0, .. })));
    assert!(matches!(open_rasterizer(Format::Pdf, b"%PDF-1.7 garbage".to_vec(), 10), Err(IngestError::Corrupt(_))));
    assert!(matches!(open_rasterizer(Format::Pdf, bytes, 0), Err(IngestError::TooManyPages(1))));
}

fn make_tiff(frames: u32, dpi: u32) -> Vec<u8> {
    use tiff::encoder::{colortype::Gray8, Rational, TiffEncoder};
    use tiff::tags::ResolutionUnit;
    let mut out = Cursor::new(Vec::new());
    let mut enc = TiffEncoder::new(&mut out).unwrap();
    for i in 0..frames {
        let (w, h) = (300, 400);
        let mut img = enc.new_image::<Gray8>(w, h).unwrap();
        img.resolution(ResolutionUnit::Inch, Rational { n: dpi, d: 1 });
        img.write_data(&vec![(i * 50) as u8; (w * h) as usize]).unwrap();
    }
    out.into_inner()
}

#[test]
fn multi_page_tiff() {
    let bytes = make_tiff(3, 150);
    assert_eq!(detect_format(&bytes), Some(Format::Tiff));
    let mut r = open_rasterizer(Format::Tiff, bytes, 10).unwrap();
    assert_eq!(r.page_count(), 3);

    // Source is 150 dpi: rendering at 200 keeps it, at 75 halves it.
    let full = r.render(2, 200).unwrap();
    assert_eq!((full.width_px, full.height_px, full.dpi), (300, 400, 150));
    let half = r.render(3, 75).unwrap();
    assert_eq!((half.width_px, half.height_px, half.dpi), (150, 200, 75));

    let a = r.page(1, 200, 100).unwrap();
    assert_eq!(a.thumbnail.height_px, 100);
    assert!(a.text_layer.spans.is_empty());
    assert_eq!(a.signals.image_count, 1);
    assert!(matches!(r.render(4, 200), Err(IngestError::Render { page: 4, .. })));
}

#[test]
fn png_single_page() {
    let mut png = Vec::new();
    DynamicImage::ImageLuma8(GrayImage::from_pixel(100, 50, image::Luma([0])))
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let mut r = open_rasterizer(Format::Png, png, 10).unwrap();
    assert_eq!(r.page_count(), 1);
    let img = r.render(1, 300).unwrap();
    assert_eq!((img.width_px, img.height_px, img.dpi), (100, 50, 300));
    assert!(matches!(open_rasterizer(Format::Png, vec![0x89, b'P', b'N', b'G'], 10), Err(IngestError::Corrupt(_))));
}

