//! Writes a small sample PDF for local runs:
//! `cargo run -p vs-ingest --example sample_pdf -- sample.pdf`
//! Pages 1-2 are born-digital text; page 3 is a "scan" (full-page image) with an
//! invisible OCR text layer.

use image::{DynamicImage, GrayImage, Luma};
use pdfium_render::prelude::*;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sample.pdf".into());
    let pdfium = vs_ingest::pdfium().expect("libpdfium (run inside `devenv shell`)");
    let mut doc = pdfium.create_new_pdf().unwrap();
    let font = doc.fonts_mut().helvetica();
    let bold = doc.fonts_mut().helvetica_bold();

    let pages: [(&str, &[&str]); 2] = [
        ("1 Introduction", &["Document parsing turns page images into structured text.", "We evaluate layout models on scientific papers."]),
        ("2 Method", &["The router selects a workflow per page class.", "Born-digital pages skip OCR and use the text layer."]),
    ];
    for (heading, lines) in pages {
        let mut page = doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
        let objs = page.objects_mut();
        objs.create_text_object(PdfPoints::new(72.0), PdfPoints::new(760.0), heading, bold, PdfPoints::new(16.0)).unwrap();
        for (i, line) in lines.iter().enumerate() {
            objs.create_text_object(PdfPoints::new(72.0), PdfPoints::new(720.0 - 18.0 * i as f32), *line, font, PdfPoints::new(11.0))
                .unwrap();
        }
    }

    let mut page = doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
    let (w, h) = (page.width(), page.height());
    let mut scan = GrayImage::from_pixel(620, 877, Luma([245]));
    for y in 80..90 {
        for x in 70..400 {
            scan.put_pixel(x, y, Luma([20])); // a dark "text line"
        }
    }
    page.objects_mut().create_image_object(PdfPoints::ZERO, PdfPoints::ZERO, &DynamicImage::ImageLuma8(scan), Some(w), Some(h)).unwrap();
    let mut ocr = PdfPageTextObject::new(&doc, "Scanned appendix text", font, PdfPoints::new(11.0)).unwrap();
    ocr.translate(PdfPoints::new(72.0), PdfPoints::new(760.0)).unwrap();
    ocr.set_render_mode(PdfPageTextRenderMode::Invisible).unwrap();
    page.objects_mut().add_text_object(ocr).unwrap();
    drop(page);

    doc.save_to_file(&out).unwrap();
    println!("wrote {out}");
}
