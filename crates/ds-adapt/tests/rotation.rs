//! Do the perturbed labels describe the pixels pdfium actually produced?
//!
//! `dataset::place` derives where a box lands from the PDF specification's reading of
//! `/Rotate`, and `src/dataset.rs` unit-tests that derivation against itself. That is not
//! the same thing as being right: the renderer is the authority, and a transform with a
//! sign error would label every rotated page in the wrong place, score every DAG near
//! zero on them, and read as "rotated pages cannot be parsed".
//!
//! So this renders the generated pages and looks at the ink. It is the test that makes
//! the rotation perturbation trustworthy, and it is the reason the perturbation was worth
//! adding at all: `rotated` is rank 0 in `select`'s class back-off ladder and had never
//! had a single example.
//!
//! The second test asks the same question of **skew**, and asks it of the production
//! estimator rather than of the geometry: `page.deskew` has to measure the angle off the
//! ink, so the only honest check is to call the function it calls.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ds_adapt::dataset::{self, Perturb, Spec};
use ds_ingest::{ChildCommand, Format, PoolConfig, RenderPool};

/// Whether pdfium can be bound at all; without it nothing here can render, and the test
/// says so and returns rather than failing — the convention `ds-ingest`'s own tests use.
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
    let mut dark = 0u64;
    let mut total = 0u64;
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
async fn the_rotated_labels_land_on_the_rendered_ink() {
    if !pdfium_or_skip() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("ds-adapt-rotation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // One document per angle, per shape.
    let spec = Spec { docs_per_shape: 4, pages_per_doc: 1, holdout_share: 0.0, perturb: Perturb::rotations(), ..Default::default() };
    let written = dataset::write(&dir, &spec).expect("write the corpus");

    let pool = Arc::new(RenderPool::new(
        ChildCommand { program: render_child(), args: vec![], env: vec![] },
        PoolConfig { size: 2, page_timeout: Duration::from_secs(60), open_timeout: Duration::from_secs(60) },
    ));

    let manifest = std::fs::read_to_string(&written.manifest).expect("manifest");
    let mut checked = 0usize;
    let mut detected: Vec<(u64, u16)> = Vec::new();
    for line in manifest.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("a manifest entry");
        let uri = v["uri"].as_str().expect("uri");
        let deg = v["rotate_deg"].as_u64().expect("the angle is recorded");
        let gt_uri = v["ground_truth_uri"].as_str().expect("labelled");

        let assets = pool.page(std::path::Path::new(uri), Format::Pdf, 10, 1, 150, 64).await.expect("render");
        let page = ds_adapt::score::load_ground_truth(gt_uri, 1).expect("readable").expect("page 1");

        // 1. The raster's shape must match what the labels were normalised against. A
        //    quarter turn that forgot to swap the page's dimensions fails here.
        let (gt_w, gt_h) = (page.width_px.expect("width"), page.height_px.expect("height"));
        let rendered_landscape = assets.image.width_px > assets.image.height_px;
        assert_eq!(
            rendered_landscape,
            gt_w > gt_h,
            "{uri} at {deg}°: the renderer produced {}x{} but the labels assume {gt_w}x{gt_h}",
            assets.image.width_px,
            assets.image.height_px
        );

        // 2. The ink must be where the labels say. This is the part that checks the
        //    transform against pdfium rather than against its own derivation.
        let img = image::load_from_memory(&assets.image.bytes).expect("the rendered page decodes").to_luma8();
        let blocks = ds_adapt::score::gt_blocks(&page);
        assert!(!blocks.is_empty(), "{uri}: no labels");

        let mut inked = 0usize;
        for b in &blocks {
            let bb = [b.bbox.x0, b.bbox.y0, b.bbox.x1, b.bbox.y1];
            if ink_fraction(&img, bb) > 0.01 {
                inked += 1;
            }
        }
        let share = inked as f64 / blocks.len() as f64;
        assert!(
            share >= 0.8,
            "{uri} at {deg}°: only {inked} of {} labelled boxes contain ink, so the rotation transform is wrong",
            blocks.len()
        );

        // 3. And a strip the labels claim is empty must be empty, or "the labels contain
        //    ink" would be satisfied by a page that is simply black.
        let margin = ink_fraction(&img, [0.0, 0.0, 1.0, 0.04]);
        assert!(margin < 0.01, "{uri} at {deg}°: the top margin has ink ({margin:.3}), so the labels are shifted");

        // 4. What the *classifier* sees, which is the whole purpose of the exercise:
        //    `rotation_deg` is what `select`'s rank-0 `rotated` condition keys on, and
        //    the conditioner templates exist to repair it.
        eprintln!(
            "{:<16} /Rotate {deg:<4} rendered {}x{}  signals: rotation_deg={} coverage={:.2}",
            uri.rsplit('/').next().unwrap_or(uri),
            assets.image.width_px,
            assets.image.height_px,
            assets.signals.rotation_deg,
            assets.signals.text_layer_coverage,
        );
        detected.push((deg, assets.signals.rotation_deg));

        checked += 1;
    }
    assert_eq!(checked, 16, "four shapes at four angles");

    // And the classifier reports the angle, which is the whole point of the exercise:
    // `rotation_deg` is what `select`'s rank-0 `rotated` condition keys on — the first
    // condition its class ladder drops — and before this perturbation existed no page in
    // the corpus had ever carried a non-zero one, so that level of the ladder had no
    // data and `page.orient` had nothing to repair.
    //
    // Asserted rather than reported because it was measured first: 16 of 16, exactly.
    // The angle comes from the document's own `/Rotate` through the renderer, not from
    // pixel analysis, which is why it is reliable enough to gate on.
    let disagreeing: Vec<&(u64, u16)> = detected.iter().filter(|(asked, got)| *asked as u16 != *got).collect();
    assert!(
        disagreeing.is_empty(),
        "the classifier did not see the rotation it was given, so the `rotated` class gets no data: {disagreeing:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// And does `page.deskew`'s own estimator see the skew the generator put in?
///
/// The skew perturbation is only worth having if the step it exists for can measure it.
/// `skew_angle` is the function `page.deskew` calls, so asking it directly is asking the
/// production code — and it answers the one question the label geometry cannot: whether
/// the *ink* is tilted, rather than whether the boxes were moved.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "renders pages; needs PDFIUM_DYNAMIC_LIB_PATH from devenv"]
async fn the_production_deskew_estimator_finds_the_generated_skew() {
    if !pdfium_or_skip() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("ds-adapt-skew-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Two skews either side of zero and no rotation, so the sign is checked too: a sign
    // error would have `page.deskew` make every page *more* crooked, which no label
    // geometry would notice.
    let spec = Spec { docs_per_shape: 2, pages_per_doc: 1, perturb: Perturb { rotate_deg: Vec::new(), skew_deg: vec![-2.0, 2.0] }, ..Default::default() };
    let written = dataset::write(&dir, &spec).expect("writes the corpus");
    let ceiling = written.skew_label_iou.expect("a skewed corpus reports its label ceiling");
    assert!(ceiling < 1.0, "a skewed label cannot be exact");

    let pool = Arc::new(RenderPool::new(
        ChildCommand { program: render_child(), args: vec![], env: vec![] },
        PoolConfig { size: 2, page_timeout: Duration::from_secs(60), open_timeout: Duration::from_secs(60) },
    ));

    let mut checked = 0;
    for line in std::fs::read_to_string(&written.manifest).expect("manifest").lines() {
        let entry: serde_json::Value = serde_json::from_str(line).expect("json");
        let truth = entry["skew_deg"].as_f64().expect("skew_deg") as f32;
        let uri = PathBuf::from(entry["uri"].as_str().expect("uri"));
        let page = pool.page(&uri, Format::Pdf, 8, 1, 200, 256).await.unwrap_or_else(|e| panic!("{}: {e}", uri.display()));
        let gray = image::load_from_memory(&page.image.bytes).expect("decodes").to_luma8();
        // The step's own search window and step size.
        let found = ds_ingest::preprocess::skew_angle(&gray, 5.0, 0.25);
        // The two signs coincide, and it is worth saying why rather than leaving the
        // next reader to rediscover it: `skew_angle` returns the *correction* to apply in
        // **image** space, while the generator's angle turns the page in **PDF** space,
        // and the y flip between the two reverses the sense. So a page generated at -2°
        // is a page the production step wants to correct by -2°.
        assert!(
            (found - truth).abs() < 0.75,
            "{}: generated {truth}° of skew, the production estimator wants to correct by {found}°",
            uri.display()
        );
        assert!(found.abs() > 0.5, "{}: a {truth}° page must read as skewed at all, got {found}", uri.display());
        checked += 1;
    }
    assert_eq!(checked, 8, "four shapes at two skews");

    std::fs::remove_dir_all(&dir).ok();
}
