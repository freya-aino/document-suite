//! Turns a generated born-digital corpus into a scanned one.
//!
//! This is the perturbation the methodology review ranked first, and the one that makes
//! `region.ocr` versus `blocks.merge_native` a real contest: today every generated page
//! has a perfect text layer, so a DAG that reads the text layer wins by construction and
//! the recognition half of Stage 1 is never scored against anything.
//!
//! **It is a separate module, and a separate pass, on purpose.** `dataset::write` is a
//! pure function — the same spec produces byte-identical PDFs — and several tests plus a
//! CI job depend on that. Rendering needs pdfium and a child process, so making `write`
//! do it would change what the generator *is*. Instead `write` runs first and this runs
//! over its output: `ds-adapt dataset --scan-dpi 200` is the two passes in one command,
//! and without the flag nothing here executes and nothing needs pdfium.
//!
//! **The labels stay mechanically exact**, which is the property that makes this corpus
//! worth having. Every bounding box is normalised to the page, and this pass changes no
//! geometry: it renders the page the generator wrote, degrades the *pixels*, and wraps
//! the result back up as a page of the same size. What changes is what the pipeline can
//! *see*: `born_digital` becomes false and `text_layer_coverage` drops to zero, because
//! an image-only page has no text layer to read.
//!
//! Rotation composes with it. A page the generator wrote with `/Rotate 90` renders in
//! reading orientation — pdfium applies the rotation — so the raster is rotated back
//! before it is wrapped, and the new page carries the same `/Rotate`. The scanned
//! document is then the same page, in the same frame, with the same labels; only the
//! glyphs have become ink.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{DynamicImage, GrayImage};
use serde::{Deserialize, Serialize};

use ds_ingest::{Format, RenderPool};

/// How badly the page is scanned.
///
/// Deterministic in every term: the blur is a fixed kernel, the noise comes from a
/// counter-seeded generator, and the JPEG quality is fixed. Two runs of the same spec
/// produce identical bytes, so a difference in a score is a difference in the code.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Scan {
    /// Render resolution. 200 is the low end of what a document scanner produces and the
    /// value the rest of the suite defaults to, so a page here is no cleaner than one a
    /// deployment would receive.
    pub dpi: u32,
    /// Gaussian blur sigma in pixels, standing in for optics and for the resampling a
    /// scanner's own pipeline does. 0 disables it.
    pub blur_sigma: f32,
    /// Peak amplitude of the additive sensor noise, in grey levels. 0 disables it.
    pub noise: u8,
    /// JPEG quality the page is re-encoded at. The blocking artefacts of a low-quality
    /// scan are the part of this that a model actually has to cope with; a lossless
    /// pipeline would be a cleaner page than most real ones.
    pub jpeg_quality: u8,
}

impl Default for Scan {
    fn default() -> Self {
        Self { dpi: 200, blur_sigma: 0.6, noise: 8, jpeg_quality: 60 }
    }
}

/// What the pass rewrote, for the caller to print.
#[derive(Debug, Clone, Serialize)]
pub struct Scanned {
    pub manifest: PathBuf,
    pub documents: usize,
    pub pages: usize,
    /// Scanned pages per shape, keyed as the manifest's document ids are prefixed.
    pub by_shape: BTreeMap<String, usize>,
}

/// Replaces every document in `written`'s manifest with a scanned copy of itself.
///
/// The PDFs are left on disk under their own names and the scans are written beside them
/// as `<id>.scan.pdf`; the manifest is rewritten in place to point at the scans, with
/// `born_digital: false` recorded so a report can say which corpus a number came from.
/// The ground truth is untouched, because none of it moved.
pub async fn scan(manifest: &Path, pool: &RenderPool, s: &Scan) -> anyhow::Result<Scanned> {
    anyhow::ensure!(s.dpi >= 72, "a scan below 72 dpi is not a document, got {}", s.dpi);
    anyhow::ensure!((1..=100).contains(&s.jpeg_quality), "jpeg_quality must be within 1..=100, got {}", s.jpeg_quality);
    anyhow::ensure!(s.blur_sigma >= 0.0 && s.blur_sigma.is_finite(), "blur_sigma must be finite and non-negative");

    let body = std::fs::read_to_string(manifest)?;
    let mut out_lines = Vec::new();
    let mut by_shape: BTreeMap<String, usize> = BTreeMap::new();
    let mut pages_total = 0usize;

    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let mut entry: serde_json::Value = serde_json::from_str(line)?;
        let uri = entry["uri"].as_str().ok_or_else(|| anyhow::anyhow!("manifest line without a uri: {line}"))?.to_string();
        let src = PathBuf::from(&uri);
        let rotate_deg = entry["rotate_deg"].as_u64().unwrap_or(0) as u16;

        let n = pool.page_count(&src, Format::Pdf, 64).await?;
        let mut pages: Vec<(GrayImage, u32)> = Vec::with_capacity(n as usize);
        for page_no in 1..=n {
            let assets = pool.page(&src, Format::Pdf, 64, page_no, s.dpi, 512).await?;
            let rendered = image::load_from_memory(&assets.image.bytes)?;
            // Back into the page's own frame, so `/Rotate` can carry the rotation the
            // way it does on the born-digital path. pdfium applied it on the way out;
            // these are exact quarter turns, so nothing is resampled.
            let unrotated = match rotate_deg % 360 {
                90 => rendered.rotate270(),
                180 => rendered.rotate180(),
                270 => rendered.rotate90(),
                _ => rendered,
            };
            pages.push((degrade(&unrotated, s), page_no));
        }

        let scan_path = src.with_extension("scan.pdf");
        std::fs::write(&scan_path, scanned_pdf(&pages.iter().map(|(p, _)| p).collect::<Vec<_>>(), rotate_deg, s.jpeg_quality)?)?;

        entry["uri"] = serde_json::Value::String(scan_path.to_string_lossy().into_owned());
        // Recorded rather than inferred: the whole point of the pass is that these two
        // facts about the input changed, and a report that groups by them can then say
        // whether a DAG's advantage was an advantage on scans or on clean PDFs.
        entry["born_digital"] = serde_json::Value::Bool(false);
        entry["scan_dpi"] = serde_json::Value::from(s.dpi);
        out_lines.push(entry.to_string());

        let shape = src.file_stem().map(|f| f.to_string_lossy().split('-').next().unwrap_or("other").to_string()).unwrap_or_else(|| "other".into());
        *by_shape.entry(shape).or_default() += n as usize;
        pages_total += n as usize;
    }

    let documents = out_lines.len();
    std::fs::write(manifest, out_lines.iter().map(|l| format!("{l}\n")).collect::<String>())?;
    Ok(Scanned { manifest: manifest.to_path_buf(), documents, pages: pages_total, by_shape })
}

/// Blur, noise, and nothing that moves a pixel somewhere else.
///
/// Every operation here is point-wise or symmetric, so a label that was correct for the
/// render is correct for the degraded copy. A perspective warp or a fold would not be,
/// which is why neither is here.
fn degrade(page: &DynamicImage, s: &Scan) -> GrayImage {
    // Grey, because a scan of a black-and-white document is grey and because one channel
    // is a third of the JPEG to carry around.
    let mut grey = page.to_luma8();
    if s.blur_sigma > 0.0 {
        grey = image::imageops::blur(&grey, s.blur_sigma);
    }
    if s.noise > 0 {
        // A counter-seeded generator rather than a thread-local one: the corpus has to be
        // reproducible, and "the noise was different this time" is indistinguishable from
        // a regression when a score moves by a point.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let amp = s.noise as i32;
        for px in grey.pixels_mut() {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let n = ((state >> 33) % (2 * amp as u64 + 1)) as i32 - amp;
            px.0[0] = (px.0[0] as i32 + n).clamp(0, 255) as u8;
        }
    }
    grey
}

/// A PDF whose pages are nothing but a JPEG each.
///
/// Written here rather than reusing `dataset::pdf_bytes` because that one builds a
/// content stream out of a `String` and a JPEG is not text. The structure is the same
/// otherwise, and deliberately minimal: a catalogue, a page tree, and per page an image
/// XObject with `/DCTDecode` plus the four-operator content stream that paints it over
/// the whole MediaBox.
fn scanned_pdf(pages: &[&GrayImage], rotate_deg: u16, quality: u8) -> anyhow::Result<Vec<u8>> {
    // The page box stays what the generator used, so a normalised label still lands where
    // it did. The raster's own pixel dimensions do not enter: the image is painted to
    // fill the box, which is what a scanner's output is.
    const PAGE_W: f32 = 595.0;
    const PAGE_H: f32 = 842.0;

    let n = pages.len();
    // 1 catalogue, 2 page tree, then per page: page, content, image.
    let page_obj = |i: usize| 3 + i * 3;
    let content_obj = |i: usize| 4 + i * 3;
    let image_obj = |i: usize| 5 + i * 3;

    enum Body {
        Text(String),
        /// A dictionary and the raw stream bytes it describes.
        Stream(String, Vec<u8>),
    }

    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_obj(i))).collect();
    let mut objects: Vec<Body> = vec![
        Body::Text("<< /Type /Catalog /Pages 2 0 R >>".into()),
        Body::Text(format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" "))),
    ];

    for (i, page) in pages.iter().enumerate() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut std::io::Cursor::new(&mut jpeg), quality)
            .encode(page.as_raw(), page.width(), page.height(), image::ExtendedColorType::L8)?;

        objects.push(Body::Text(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] /Rotate {rotate_deg} \
             /Resources << /XObject << /Im0 {} 0 R >> >> /Contents {} 0 R >>",
            image_obj(i),
            content_obj(i)
        )));
        // `cm` scales the unit square the image is drawn in up to the whole page.
        let content = format!("q {PAGE_W} 0 0 {PAGE_H} 0 0 cm /Im0 Do Q\n");
        objects.push(Body::Stream(format!("<< /Length {} >>", content.len()), content.into_bytes()));
        objects.push(Body::Stream(
            format!(
                "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceGray \
                 /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>",
                page.width(),
                page.height(),
                jpeg.len()
            ),
            jpeg,
        ));
    }

    let mut out: Vec<u8> = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        match body {
            Body::Text(s) => out.extend_from_slice(s.as_bytes()),
            Body::Stream(dict, bytes) => {
                out.extend_from_slice(dict.as_bytes());
                out.extend_from_slice(b"\nstream\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\nendstream");
            }
        }
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n", objects.len() + 1).as_bytes());
    Ok(out)
}

/// A render pool for the scan pass, built the way `ds-adapt`'s other commands build one.
pub fn render_pool(child: PathBuf, size: usize) -> Arc<RenderPool> {
    Arc::new(RenderPool::new(
        ds_ingest::ChildCommand { program: child, args: vec![], env: vec![] },
        ds_ingest::PoolConfig {
            size,
            page_timeout: std::time::Duration::from_secs(60),
            open_timeout: std::time::Duration::from_secs(60),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: u32, h: u32, v: u8) -> GrayImage {
        GrayImage::from_pixel(w, h, image::Luma([v]))
    }

    #[test]
    fn a_scanned_page_is_a_pdf_carrying_a_jpeg() {
        let page = flat(64, 90, 200);
        let bytes = scanned_pdf(&[&page], 0, 60).expect("writes");
        assert!(bytes.starts_with(b"%PDF-1.7"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Subtype /Image") && text.contains("/Filter /DCTDecode"), "the page must be an image");
        assert!(text.contains("/ColorSpace /DeviceGray"));
        // And no text at all, which is the point: this is what makes `born_digital`
        // false and gives `blocks.merge_native` nothing to read.
        assert!(!text.contains(" Tj"), "a scanned page has no text operators");
        // The JPEG itself is in there, as bytes rather than as an escaped string.
        assert!(bytes.windows(2).any(|w| w == [0xFF, 0xD8]), "no JPEG SOI marker");
    }

    #[test]
    fn the_page_box_does_not_depend_on_the_raster_size() {
        // A label is normalised to the page, so the box has to stay the generator's
        // regardless of the dpi the scan was rendered at — otherwise every bbox would be
        // wrong by the ratio of the two and it would read as a parser failure.
        let small = scanned_pdf(&[&flat(32, 45, 128)], 0, 50).expect("writes");
        let large = scanned_pdf(&[&flat(320, 450, 128)], 0, 50).expect("writes");
        for bytes in [&small, &large] {
            assert!(String::from_utf8_lossy(bytes).contains("/MediaBox [0 0 595 842]"));
        }
    }

    #[test]
    fn rotation_is_declared_rather_than_baked_in() {
        let bytes = scanned_pdf(&[&flat(32, 45, 128)], 90, 50).expect("writes");
        assert!(String::from_utf8_lossy(&bytes).contains("/Rotate 90"), "the scan must carry the page's own rotation");
    }

    #[test]
    fn multiple_pages_each_get_their_own_image() {
        let bytes = scanned_pdf(&[&flat(16, 22, 10), &flat(16, 22, 240)], 0, 50).expect("writes");
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text.matches("/Subtype /Image").count(), 2);
        assert_eq!(text.matches("/Type /Page ").count(), 2);
        assert!(text.contains("/Count 2"));
    }

    #[test]
    fn degrading_is_reproducible_and_actually_degrades() {
        // Reproducible, because "the noise was different this time" is indistinguishable
        // from a regression when a composite moves by a point.
        let page = DynamicImage::ImageLuma8(flat(48, 48, 128));
        let s = Scan::default();
        assert_eq!(degrade(&page, &s).into_raw(), degrade(&page, &s).into_raw());
        // And it is not the identity: a flat grey field comes back noisy.
        let noisy = degrade(&page, &s);
        assert!(noisy.pixels().any(|p| p.0[0] != 128), "the noise did nothing");
        // Off, it is the identity on a flat field, so the knobs can be turned down to
        // isolate which degradation a DAG is losing to.
        let clean = degrade(&page, &Scan { blur_sigma: 0.0, noise: 0, ..s });
        assert!(clean.pixels().all(|p| p.0[0] == 128));
    }

    #[test]
    fn a_scan_that_is_not_a_document_is_refused() {
        // `scan` validates before it renders anything, which is where the cost is.
        let bad = [Scan { dpi: 10, ..Scan::default() }, Scan { jpeg_quality: 0, ..Scan::default() }, Scan { blur_sigma: -1.0, ..Scan::default() }];
        for s in bad {
            let pool = render_pool(PathBuf::from("/nonexistent"), 1);
            let err = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(scan(Path::new("/nonexistent/manifest.jsonl"), &pool, &s))
                .expect_err("must be refused");
            let msg = err.to_string();
            assert!(!msg.contains("No such file"), "validation must come before any I/O, got {msg:?}");
        }
    }
}
