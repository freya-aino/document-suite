//! Does the scanned corpus actually behave like a scan, and do the labels still fit it?
//!
//! `src/scan.rs` unit-tests the file it writes — that the page is an image, that the box
//! is the generator's, that the degradation is reproducible. None of that is the claim
//! that matters. The claim is:
//!
//! 1. the pipeline sees **no text layer**, so `blocks.merge_native` has nothing to read
//!    and `region.ocr` is in a real contest for the first time;
//! 2. the ground-truth boxes still land on the ink, at every rotation, so a score on the
//!    scanned corpus is a score and not an artefact of a transform.
//!
//! Both are questions about what pdfium produces, so both are answered by rendering.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ds_adapt::dataset::{self, Perturb, Spec};
use ds_adapt::scan::{self, Scan};
use ds_ingest::{ChildCommand, Format, PoolConfig, RenderPool};

fn pdfium_or_skip() -> bool {
    if std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_some() {
        return true;
    }
    eprintln!("skipping: PDFIUM_DYNAMIC_LIB_PATH is unset (devenv provides it)");
    false
}

fn render_child() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.to_path_buf()))
        .expect("the test binary has a parent directory");
    let path = dir.join("ds-render-child");
    assert!(path.exists(), "{} is missing; build the workspace before running this test", path.display());
    path
}

/// Fraction of pixels darker than mid-grey inside a normalised box.
fn ink_fraction(img: &image::GrayImage, b: [f32; 4]) -> f64 {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x0 = (b[0] * w).floor().max(0.0) as u32;
    let y0 = (b[1] * h).floor().max(0.0) as u32;
    let x1 = (b[2] * w).ceil().min(w) as u32;
    let y1 = (b[3] * h).ceil().min(h) as u32;
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let (mut dark, mut total) = (0u64, 0u64);
    for y in y0..y1 {
        for x in x0..x1 {
            total += 1;
            if img.get_pixel(x, y)[0] < 128 {
                dark += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        dark as f64 / total as f64
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "renders pages; needs PDFIUM_DYNAMIC_LIB_PATH from devenv"]
async fn a_scanned_page_has_no_text_layer_and_still_fits_its_labels() {
    if !pdfium_or_skip() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("ds-adapt-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Both rotations, because the scan pass has to undo pdfium's rotation and re-declare
    // it — a sign error there would put every label on a rotated scan in the wrong place
    // and read as "scans cannot be parsed".
    let spec = Spec { docs_per_shape: 2, pages_per_doc: 1, perturb: Perturb { rotate_deg: vec![0, 90], skew_deg: Vec::new() }, ..Default::default() };
    let written = dataset::write(&dir, &spec).expect("writes the corpus");

    let pool = Arc::new(RenderPool::new(
        ChildCommand { program: render_child(), args: vec![], env: vec![] },
        PoolConfig { size: 2, page_timeout: Duration::from_secs(60), open_timeout: Duration::from_secs(60) },
    ));

    // First: the born-digital original has a text layer. Without this the assertion
    // below would also pass if the renderer simply never reported spans.
    let before: Vec<serde_json::Value> =
        std::fs::read_to_string(&written.manifest).expect("manifest").lines().map(|l| serde_json::from_str(l).expect("json")).collect();
    let first = PathBuf::from(before[0]["uri"].as_str().expect("uri"));
    let native = pool.page(&first, Format::Pdf, 8, 1, 150, 256).await.expect("renders the original");
    assert!(!native.text_layer.spans.is_empty(), "the generated corpus is supposed to be born-digital");
    assert!(native.signals.text_chars > 0, "and to have characters the classifier can count");
    assert_eq!(native.signals.image_count, 0, "and no images at all");

    let s = Scan { dpi: 150, ..Scan::default() };
    let scanned = scan::scan(&written.manifest, &pool, &s).await.expect("scans the corpus");
    assert_eq!(scanned.documents, before.len(), "every document gets a scan");

    let after: Vec<serde_json::Value> =
        std::fs::read_to_string(&written.manifest).expect("manifest").lines().map(|l| serde_json::from_str(l).expect("json")).collect();
    let mut checked = 0;
    for (entry, original) in after.iter().zip(&before) {
        assert_eq!(entry["born_digital"], serde_json::json!(false), "the manifest must say what the corpus now is");
        assert_eq!(entry["rotate_deg"], original["rotate_deg"], "the rotation is preserved, not consumed");
        let uri = PathBuf::from(entry["uri"].as_str().expect("uri"));
        let rotate = entry["rotate_deg"].as_u64().unwrap_or(0);

        let page = pool.page(&uri, Format::Pdf, 8, 1, 150, 256).await.unwrap_or_else(|e| panic!("{}: {e}", uri.display()));
        // The claim this whole pass exists for.
        assert!(page.text_layer.spans.is_empty(), "{}: a scan must have no text layer, got {} spans", uri.display(), page.text_layer.spans.len());
        assert_eq!(page.signals.text_chars, 0, "{}: and no characters for the classifier to count", uri.display());
        assert!(page.signals.text_layer_coverage < 0.01, "{}: coverage {}", uri.display(), page.signals.text_layer_coverage);
        // The other half of what makes a page a scan: one image, covering it.
        assert_eq!(page.signals.image_count, 1, "{}: a scan is one image", uri.display());
        assert!(page.signals.image_area_ratio > 0.9, "{}: image area {}", uri.display(), page.signals.image_area_ratio);
        // And the rotation survives as the page's own `/Rotate`, which is what lets the
        // `rotated` class keep its examples on the scanned corpus too.
        assert_eq!(page.signals.rotation_deg as u64, rotate, "{}: rotation lost", uri.display());

        let img = image::load_from_memory(&page.image.bytes).expect("decodes").to_luma8();
        // The rendered scan has the same orientation as the original did, so the labels
        // the generator wrote for that rotation still apply.
        let (w, h) = (img.width(), img.height());
        if rotate % 180 == 90 {
            assert!(w > h, "{}: a 90° page renders landscape, got {w}x{h}", uri.display());
        } else {
            assert!(h > w, "{}: an upright page renders portrait, got {w}x{h}", uri.display());
        }

        let gt_path = PathBuf::from(original["ground_truth_uri"].as_str().expect("ground_truth_uri"));
        let gt: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&gt_path).expect("ground truth")).expect("json");
        let blocks = gt["pages"][0]["blocks"].as_array().expect("blocks");
        assert!(!blocks.is_empty());
        let mut inked = 0;
        for b in blocks {
            let bb = b["bbox"].as_array().expect("bbox");
            let bbox = [
                bb[0].as_f64().expect("x0") as f32,
                bb[1].as_f64().expect("y0") as f32,
                bb[2].as_f64().expect("x1") as f32,
                bb[3].as_f64().expect("y1") as f32,
            ];
            if ink_fraction(&img, bbox) > 0.005 {
                inked += 1;
            }
        }
        // Not every block: a scan is blurred, noised and JPEG-quantised, and a one-line
        // label on a form can come out lighter than the threshold. Most of them is the
        // claim that distinguishes "the labels fit" from "the transform has a sign
        // error", which would put *none* of them on ink.
        assert!(
            inked * 2 > blocks.len(),
            "{}: only {inked} of {} labelled boxes contain ink at rotation {rotate}",
            uri.display(),
            blocks.len()
        );
        checked += 1;
    }
    assert_eq!(checked, 8, "two shapes' worth of documents at two rotations");

    std::fs::remove_dir_all(&dir).ok();
}
