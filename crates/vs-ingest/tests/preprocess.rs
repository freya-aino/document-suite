//! The pre-processing steps end to end: a page image in, a page image out.

use std::sync::Arc;

use serde_json::json;

use vs_core::{ClassifierTier, DocType, HashKey, LanguageTag, PageProfile, RunId};
use vs_dag::{Artifact, ArtifactKind, PageImage, Step, StepContext};
use vs_ingest::preprocess::{Binarize, CropMargins, Denoise, Deskew, Normalize};

fn ctx() -> StepContext {
    StepContext {
        run_id: RunId::new(),
        document_hk: HashKey::of(&["d"]),
        page_hk: HashKey::of(&["p"]),
        page_no: 1,
        profile: PageProfile {
            language: LanguageTag::parse("en").unwrap(),
            doc_type: DocType::Other,
            born_digital: false,
            text_layer_coverage: 0.0,
            has_handwriting: false,
            has_images: true,
            has_tables: false,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 0.5,
            tier: ClassifierTier::Rules,
        },
        dag_ref: "d@1".into(),
        node_id: "pre".into(),
    }
}

/// A page with horizontal text-like bands inside a blank margin.
fn page_png(w: u32, h: u32) -> PageImage {
    let img = image::GrayImage::from_fn(w, h, |x, y| {
        let in_margin = x < w / 10 || x > w * 9 / 10;
        let on_line = (y / 4) % 3 == 0 && y > h / 10 && y < h * 9 / 10;
        image::Luma([if on_line && !in_margin { 40 } else { 230 }])
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    PageImage { uri: "mem://page".into(), bytes: Arc::from(bytes), width_px: w, height_px: h, dpi: 200 }
}

async fn run(step: &dyn Step, page: &PageImage, params: serde_json::Value) -> PageImage {
    let inputs: Vec<&Artifact> = vec![];
    let art = Artifact::PageImage(page.clone());
    let mut refs = inputs;
    refs.push(&art);
    let out = step.run(&ctx(), &refs, &params).await.expect("step must not fail");
    assert!(out.model_ref.is_none(), "a CPU step calls no model");
    assert!(out.prompt_ref.is_none());
    match out.artifact {
        Artifact::PageImage(p) => p,
        other => panic!("expected a page image, got {other:?}"),
    }
}

#[test]
fn every_step_is_page_image_to_page_image() {
    // The shape that lets these compose, and lets a DAG apply none or several.
    for step in [
        &Deskew as &dyn Step,
        &Denoise as &dyn Step,
        &Normalize as &dyn Step,
        &Binarize as &dyn Step,
        &CropMargins as &dyn Step,
    ] {
        let sig = step.signature();
        assert_eq!(sig.inputs, vec![ArtifactKind::PageImage], "{}", step.kind());
        assert_eq!(sig.output, ArtifactKind::PageImage, "{}", step.kind());
        assert!(step.kind().starts_with("page."), "{}", step.kind());
    }
}

#[tokio::test]
async fn each_step_returns_a_decodable_page() {
    let page = page_png(300, 400);
    for step in [
        &Deskew as &dyn Step,
        &Denoise as &dyn Step,
        &Normalize as &dyn Step,
        &Binarize as &dyn Step,
        &CropMargins as &dyn Step,
    ] {
        let out = run(step, &page, serde_json::Value::Null).await;
        let decoded = image::load_from_memory(&out.bytes)
            .unwrap_or_else(|e| panic!("{} produced undecodable bytes: {e}", step.kind()));
        assert!(decoded.width() > 0 && decoded.height() > 0, "{}", step.kind());
        assert_eq!(out.dpi, page.dpi, "{} must not change the page's dpi", step.kind());
    }
}

/// The contract that lets these be wired in safely: a step that cannot improve a page
/// must not be able to destroy it.
#[tokio::test]
async fn undecodable_input_passes_through_unchanged() {
    let junk = PageImage {
        uri: "mem://junk".into(),
        bytes: Arc::from(vec![0u8, 1, 2, 3, 4]),
        width_px: 10,
        height_px: 10,
        dpi: 200,
    };
    for step in [&Deskew as &dyn Step, &Denoise as &dyn Step, &Binarize as &dyn Step] {
        let out = run(step, &junk, serde_json::Value::Null).await;
        assert_eq!(&*out.bytes, &*junk.bytes, "{} must pass the page through", step.kind());
        assert_eq!(out.uri, junk.uri, "{} must not relabel a page it did not touch", step.kind());
    }
}

#[tokio::test]
async fn binarize_produces_two_tone_greyscale() {
    let out = run(&Binarize, &page_png(200, 200), serde_json::Value::Null).await;
    let decoded = image::load_from_memory(&out.bytes).unwrap().to_luma8();
    let mut levels: Vec<u8> = decoded.pixels().map(|p| p.0[0]).collect();
    levels.sort_unstable();
    levels.dedup();
    assert_eq!(levels, vec![0, 255], "Otsu output must be exactly black and white");
    // Greyscale, not RGB: the whole point is one channel.
    assert!(matches!(image::load_from_memory(&out.bytes).unwrap(), image::DynamicImage::ImageLuma8(_)));
}

#[tokio::test]
async fn crop_margins_shrinks_the_page_but_keeps_the_content() {
    let page = page_png(400, 400);
    let out = run(&CropMargins, &page, serde_json::Value::Null).await;
    assert!(out.width_px < page.width_px, "a page with blank margins should be narrowed");
    assert!(out.height_px < page.height_px);
    // The generator leaves ~10 % margin each side; with 1 % padding kept, roughly 80 %
    // of each dimension should survive. A crop far below that means the ink bounds were
    // measured wrong.
    assert!(out.width_px > page.width_px * 7 / 10, "cropped too aggressively: {}", out.width_px);
}

/// A page with nothing to trim must be left exactly as it was, not re-encoded.
#[tokio::test]
async fn crop_margins_leaves_a_full_bleed_page_alone() {
    let w = 200;
    let img = image::GrayImage::from_fn(w, w, |x, y| image::Luma([if (x + y) % 3 == 0 { 30 } else { 220 }]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    let page = PageImage { uri: "mem://full".into(), bytes: Arc::from(bytes), width_px: w, height_px: w, dpi: 200 };
    let out = run(&CropMargins, &page, serde_json::Value::Null).await;
    assert_eq!(&*out.bytes, &*page.bytes, "nothing to crop means no re-encode");
}

#[tokio::test]
async fn deskew_below_the_threshold_does_nothing() {
    let page = page_png(300, 300);
    // A straight page measures under min_apply_deg, so it is returned untouched.
    let out = run(&Deskew, &page, json!({ "min_apply_deg": 0.2 })).await;
    assert_eq!(&*out.bytes, &*page.bytes);
}

#[tokio::test]
async fn bad_params_are_rejected_rather_than_ignored() {
    let page = page_png(100, 100);
    let art = Artifact::PageImage(page);
    let err = Deskew.run(&ctx(), &[&art], &json!({ "max_deg": "five" })).await;
    assert!(err.is_err(), "a malformed param must not be silently defaulted");
}
