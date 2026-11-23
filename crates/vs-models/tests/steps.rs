mod common;

use common::*;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::Arc;

use vs_core::{BBox, BlockContent, BlockKind, ClassifierTier, DocType, HashKey, LanguageTag, PageProfile, RunId};
use vs_dag::{Artifact, FigureCrop, LayoutRegion, PageImage, Step, StepContext, StepError, StepRegistry};
use vs_models::steps::{ChartToTable, LayoutDetect, PageParseE2e, RegionStep};
use vs_models::{ModelRegistry, OpenAiClient};

fn client(toml: &str) -> Arc<OpenAiClient> {
    Arc::new(OpenAiClient::new(ModelRegistry::from_toml(toml).unwrap()))
}

fn ctx() -> StepContext {
    StepContext {
        run_id: RunId::new(),
        document_hk: HashKey::of(&["doc"]),
        page_hk: HashKey::of(&["page"]),
        page_no: 1,
        profile: PageProfile {
            language: LanguageTag::parse("de").unwrap(),
            doc_type: DocType::Report,
            born_digital: false,
            text_layer_coverage: 0.0,
            has_handwriting: false,
            has_images: true,
            has_tables: true,
            has_formulas: true,
            has_charts: true,
            rotation_deg: 0,
            confidence: 0.9,
            tier: ClassifierTier::Rules,
        },
        dag_ref: "t@1".into(),
        node_id: "n1".into(),
        interactive: false,
        pages: None,
    }
}

/// A 200x100 page: left half red, right half blue (so crops are distinguishable).
fn page_png() -> PageImage {
    let mut img = RgbImage::new(200, 100);
    for (x, _, p) in img.enumerate_pixels_mut() {
        *p = if x < 100 { Rgb([255, 0, 0]) } else { Rgb([0, 0, 255]) };
    }
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img).write_to(&mut out, ImageFormat::Png).unwrap();
    PageImage { uri: "mem://p".into(), bytes: Arc::from(out.into_inner()), width_px: 200, height_px: 100, dpi: 72 }
}

fn region(kind: BlockKind, bbox: (f32, f32, f32, f32), ro: u32) -> LayoutRegion {
    LayoutRegion { kind, bbox: BBox::new(bbox.0, bbox.1, bbox.2, bbox.3).unwrap(), reading_order: ro, score: 0.9 }
}

/// Decodes the image sent in a request and returns the dominant channel ("red"/"blue") and size.
fn sent_image(c: &Captured) -> (&'static str, u32, u32) {
    let url = c.body["messages"][0]["content"][0]["image_url"]["url"].as_str().unwrap();
    let b64 = url.split_once("base64,").unwrap().1;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
    let (w, h) = img.dimensions();
    let p = img.get_pixel(w / 2, h / 2);
    // Green is here for the `crop_dpi` tests, where the finer render is green and which
    // bitmap a crop came from is the thing under test.
    let colour = if p[1] > p[0] && p[1] > p[2] {
        "green"
    } else if p[0] > p[2] {
        "red"
    } else {
        "blue"
    };
    (colour, w, h)
}

#[tokio::test]
async fn layout_detect_generic_json_uses_schema() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert_eq!(c.body["response_format"]["type"], "json_schema");
        assert!(c.body["response_format"]["json_schema"]["schema"]["properties"]["regions"].is_object());
        assert!(c.user_text().contains("language is de"));
        (200, chat_ok(r#"{"regions":[
            {"label":"table","bbox":[100,0,200,100],"reading_order":1,"score":0.8},
            {"label":"text","bbox":[0,0,100,100],"reading_order":0,"score":0.95}]}"#))
    }))
    .await;
    let c = client(&registry_toml("vlm", "generic_json", &[&srv.base_url], ""));
    let step = LayoutDetect { client: c };
    let out = step.run(&ctx(), &[&Artifact::PageImage(page_png())], &json!({"model": "vlm"})).await.unwrap();
    let Artifact::Layout(regions) = out.artifact else { panic!("layout") };
    assert_eq!(regions.len(), 2);
    assert_eq!((regions[0].kind, regions[0].reading_order), (BlockKind::Paragraph, 0));
    assert_eq!((regions[1].kind, regions[1].reading_order), (BlockKind::Table, 1));
    assert!((regions[1].bbox.x0 - 0.5).abs() < 1e-6, "pixel bbox normalised by page size");
    assert!((regions[0].score - 0.95).abs() < 1e-6);
    assert_eq!(out.model_ref.as_deref(), Some("vlm@r1"));
    assert!(out.prompt_ref.unwrap().starts_with("layout_detect@"));
}

#[tokio::test]
async fn layout_detect_doc_vlm_uses_family_parser_and_native_prompt() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert!(c.body.get("response_format").is_none(), "doc-VLMs are not schema-guided");
        assert_eq!(c.user_text(), "\nLayout Detection:");
        (200, chat_ok("<|box_start|>0 0 500 1000<|box_end|><|ref_start|>text<|ref_end|>\n<|box_start|>500 0 1000 1000<|box_end|><|ref_start|>table<|ref_end|>"))
    }))
    .await;
    let c = client(&registry_toml("mineru", "mineru", &[&srv.base_url], ""));
    let out = LayoutDetect { client: c }.run(&ctx(), &[&Artifact::PageImage(page_png())], &json!({"model": "mineru"})).await.unwrap();
    let Artifact::Layout(regions) = out.artifact else { panic!("layout") };
    assert_eq!(regions.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![BlockKind::Paragraph, BlockKind::Table]);
    assert!((regions[1].bbox.x0 - 0.5).abs() < 1e-6);
    assert!(out.prompt_ref.unwrap().starts_with("mineru_layout@"));
}

#[tokio::test]
async fn layout_detect_requires_model() {
    let c = client(&registry_toml("vlm", "generic_json", &["http://127.0.0.1:1/v1"], ""));
    let err = LayoutDetect { client: c }.run(&ctx(), &[&Artifact::PageImage(page_png())], &json!({})).await.unwrap_err();
    assert!(matches!(err, StepError::InvalidParams(_)));
}

/// A finer page, for the `crop_dpi` tests: four times the page's pixels, and green, so
/// which bitmap a crop came from is visible in the image the model was sent.
struct FinerPage {
    asked: Arc<std::sync::Mutex<Vec<u32>>>,
}

#[async_trait::async_trait]
impl vs_dag::PageSource for FinerPage {
    async fn page_at_dpi(&self, _page_no: u32, dpi: u32) -> Result<PageImage, vs_dag::PageSourceError> {
        self.asked.lock().expect("mutex").push(dpi);
        let mut img = RgbImage::new(800, 400);
        for (_, _, p) in img.enumerate_pixels_mut() {
            *p = Rgb([0, 255, 0]);
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img).write_to(&mut out, ImageFormat::Png).expect("encode");
        Ok(PageImage { uri: "mem://finer".into(), bytes: Arc::from(out.into_inner()), width_px: 800, height_px: 400, dpi })
    }
}

/// `crop_dpi` cuts the region from a finer render of the page.
///
/// For three revisions this param was documented and *rejected*, because a step had no
/// way back to the original document. The test is that the crop the model receives comes
/// from the finer bitmap — four times the pixels — and not from the page image.
#[tokio::test]
async fn crop_dpi_crops_from_a_finer_render_of_the_page() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let srv = MockServer::start({
        let seen = seen.clone();
        Arc::new(move |_, c| {
            let (colour, w, h) = sent_image(c);
            seen.lock().expect("mutex").push((colour, w, h));
            (200, chat_ok("text"))
        })
    })
    .await;
    let c = client(&registry_toml("vlm", "generic_json", &[&srv.base_url], ""));
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ctx = ctx();
    ctx.pages = Some(Arc::new(FinerPage { asked: asked.clone() }));

    let step = RegionStep::ocr(c);
    let layout = Artifact::Layout(vec![region(BlockKind::Paragraph, (0.0, 0.0, 0.5, 1.0), 0)]);
    let out = step
        .run(&ctx, &[&Artifact::PageImage(page_png()), &layout], &json!({"model": "vlm", "crop_dpi": 400}))
        .await
        .expect("the param is honoured now, not refused");
    assert!(matches!(out.artifact, Artifact::Blocks(ref b) if b.len() == 1));

    assert_eq!(*asked.lock().expect("mutex"), vec![400], "the step did not ask for a finer render");
    let sent = seen.lock().expect("mutex").clone();
    assert_eq!(sent.len(), 1);
    let (colour, w, h) = sent[0];
    assert_eq!(colour, "green", "the crop came from the page image, not the finer render");
    // Half the finer page's width and its full height, plus the crop padding: the bbox is
    // normalised, so the same region is four times the pixels it would have been.
    assert!((400..=420).contains(&w) && (400..=420).contains(&h), "got {w}x{h}");
}

/// And every way of *not* getting a finer render falls back to the page image rather than
/// failing the page. Each is a state a deployment can be in, not a bug.
#[tokio::test]
async fn crop_dpi_falls_back_to_the_page_image_when_there_is_nothing_finer() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let srv = MockServer::start({
        let seen = seen.clone();
        Arc::new(move |_, c| {
            seen.lock().expect("mutex").push(sent_image(c));
            (200, chat_ok("text"))
        })
    })
    .await;
    let c = client(&registry_toml("vlm", "generic_json", &[&srv.base_url], ""));
    let step = RegionStep::ocr(c);
    let layout = Artifact::Layout(vec![region(BlockKind::Paragraph, (0.0, 0.0, 0.5, 1.0), 0)]);

    // No page source at all: `worker.max_crop_dpi = 0`, or an offline run over loose
    // files. The page is still processed.
    let out = step
        .run(&ctx(), &[&Artifact::PageImage(page_png()), &layout], &json!({"model": "vlm", "crop_dpi": 400}))
        .await
        .expect("a missing capability is not a failed page");
    assert!(matches!(out.artifact, Artifact::Blocks(ref b) if b.len() == 1));

    // A page source that cannot produce one — the original is past its retention.
    struct Gone;
    #[async_trait::async_trait]
    impl vs_dag::PageSource for Gone {
        async fn page_at_dpi(&self, _: u32, _: u32) -> Result<PageImage, vs_dag::PageSourceError> {
            Err(vs_dag::PageSourceError::Unavailable("originals/t1/abc: not found".into()))
        }
    }
    let mut ctx = ctx();
    ctx.pages = Some(Arc::new(Gone));
    let out = step
        .run(&ctx, &[&Artifact::PageImage(page_png()), &layout], &json!({"model": "vlm", "crop_dpi": 400}))
        .await
        .expect("an absent original is not a failed page either");
    assert!(matches!(out.artifact, Artifact::Blocks(ref b) if b.len() == 1));

    // Both crops came from the page image: 100 x 100 of the 200 x 100 page.
    let sent = seen.lock().expect("mutex").clone();
    assert_eq!(sent.len(), 2);
    for (colour, w, h) in sent {
        assert_eq!(colour, "red", "the left half of the page image is red");
        // The page image's own pixels (100 x 100 of a 200 x 100 page), plus padding.
        assert!((100..=120).contains(&w) && (100..=110).contains(&h), "got {w}x{h}");
    }
}

#[tokio::test]
async fn region_ocr_crops_each_target_region_and_keeps_layout_geometry() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert_eq!(c.user_text(), "OCR:", "PaddleOCR-VL native prompt");
        assert_eq!(c.image_count(), 1);
        let (colour, _, _) = sent_image(c);
        (200, chat_ok(&format!("text from {colour} crop")))
    }))
    .await;
    let c = client(&registry_toml("pp", "paddleocr_vl", &[&srv.base_url], ""));
    let layout = vec![
        region(BlockKind::Paragraph, (0.5, 0.0, 1.0, 1.0), 1), // blue
        region(BlockKind::Table, (0.0, 0.0, 0.5, 1.0), 2),     // not an OCR target
        region(BlockKind::Heading, (0.0, 0.0, 0.5, 0.5), 0),   // red
    ];
    let out = RegionStep::ocr(c)
        .run(&ctx(), &[&Artifact::PageImage(page_png()), &Artifact::Layout(layout)], &json!({"model": "pp"}))
        .await
        .unwrap();
    let Artifact::Blocks(blocks) = out.artifact else { panic!("blocks") };
    assert_eq!(srv.hits(), 2, "one request per target region");
    assert_eq!(blocks.len(), 2);
    assert_eq!((blocks[0].kind, blocks[0].reading_order, blocks[0].text.as_str()), (BlockKind::Heading, 0, "text from red crop"));
    assert_eq!((blocks[1].kind, blocks[1].reading_order, blocks[1].text.as_str()), (BlockKind::Paragraph, 1, "text from blue crop"));
    assert_eq!(blocks[1].bbox, BBox::new(0.5, 0.0, 1.0, 1.0).unwrap());
    assert_eq!(blocks[0].content, BlockContent::Text { text: "text from red crop".into() });
    assert!((blocks[0].confidence - 0.9).abs() < 1e-6);
    assert_eq!(blocks[0].node_id, "n1");
    assert!(out.prompt_ref.unwrap().starts_with("paddleocr_vl_ocr@"));
    // Crop sizes: the blue region is 100x100 plus 4 px padding, clamped to the page.
    let sizes: Vec<(u32, u32)> = srv.requests().iter().map(|c| { let (_, w, h) = sent_image(c); (w, h) }).collect();
    assert!(sizes.contains(&(104, 100)), "{sizes:?}");
}

#[tokio::test]
async fn region_ocr_kinds_filter_and_prompt_override() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert!(c.user_text().starts_with("Transcribe all text"), "generic prompt for a general VLM: {}", c.user_text());
        assert!(c.user_text().contains("language is de"));
        (200, chat_ok("```\nsome table text\n```"))
    }))
    .await;
    let c = client(&registry_toml("vlm", "generic_json", &[&srv.base_url], ""));
    let layout = vec![region(BlockKind::Paragraph, (0.0, 0.0, 0.5, 1.0), 0), region(BlockKind::Table, (0.5, 0.0, 1.0, 1.0), 1)];
    let out = RegionStep::ocr(c)
        .run(&ctx(), &[&Artifact::PageImage(page_png()), &Artifact::Layout(layout)], &json!({"model": "vlm", "kinds": ["table"], "prompt": "region_ocr"}))
        .await
        .unwrap();
    let Artifact::Blocks(blocks) = out.artifact else { panic!("blocks") };
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Table);
    // The region is a table, so the plain-text answer is wrapped as table HTML and fences are stripped.
    assert!(matches!(&blocks[0].content, BlockContent::Table { html } if html.contains("some table text")));
}

#[tokio::test]
async fn table_and_formula_steps_parse_their_content() {
    let srv = MockServer::start(Arc::new(|_, c| {
        let text = c.user_text();
        if text == "Table Recognition:" {
            (200, chat_ok("<html><body><table><tr><td>a</td><td>b</td></tr></table></body></html>"))
        } else if text == "Formula Recognition:" {
            (200, chat_ok("$$\\alpha + \\beta$$"))
        } else {
            (400, json!({"error": format!("unexpected prompt {text:?}")}))
        }
    }))
    .await;
    let c = client(&registry_toml("pp", "paddleocr_vl", &[&srv.base_url], ""));
    let layout = vec![region(BlockKind::Table, (0.0, 0.0, 0.5, 1.0), 0), region(BlockKind::Formula, (0.5, 0.0, 1.0, 1.0), 1)];
    let inputs = [&Artifact::PageImage(page_png()), &Artifact::Layout(layout)];
    let t = RegionStep::table(c.clone()).run(&ctx(), &inputs, &json!({"model": "pp"})).await.unwrap();
    let Artifact::Blocks(tb) = t.artifact else { panic!() };
    assert_eq!(tb.len(), 1);
    assert_eq!(tb[0].content, BlockContent::Table { html: "<table><tr><td>a</td><td>b</td></tr></table>".into() });
    assert_eq!(tb[0].text, "a\tb");
    let f = RegionStep::formula(c).run(&ctx(), &inputs, &json!({"model": "pp"})).await.unwrap();
    let Artifact::Blocks(fb) = f.artifact else { panic!() };
    assert_eq!(fb[0].content, BlockContent::Formula { latex: "\\alpha + \\beta".into() });
    assert_eq!(fb[0].reading_order, 1);
}

#[tokio::test]
async fn region_step_with_no_matching_regions_makes_no_requests() {
    let c = client(&registry_toml("pp", "paddleocr_vl", &["http://127.0.0.1:1/v1"], ""));
    let layout = vec![region(BlockKind::Figure, (0.0, 0.0, 0.5, 1.0), 0)];
    let out = RegionStep::table(c).run(&ctx(), &[&Artifact::PageImage(page_png()), &Artifact::Layout(layout)], &json!({"model": "pp"})).await.unwrap();
    assert!(matches!(out.artifact, Artifact::Blocks(b) if b.is_empty()));
}

#[tokio::test]
async fn region_step_propagates_model_errors() {
    let srv = MockServer::start(Arc::new(|_, _| (500, json!({"error": "boom"})))).await;
    let c = client(&registry_toml("pp", "paddleocr_vl", &[&srv.base_url], ""));
    let layout = vec![region(BlockKind::Paragraph, (0.0, 0.0, 0.5, 1.0), 0)];
    let err = RegionStep::ocr(c).run(&ctx(), &[&Artifact::PageImage(page_png()), &Artifact::Layout(layout)], &json!({"model": "pp"})).await.unwrap_err();
    assert!(matches!(err, StepError::Model(_)), "{err}");
}

#[tokio::test]
async fn chart_to_table_only_handles_chart_crops() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert_eq!(c.body["response_format"]["type"], "json_schema");
        assert!(c.user_text().starts_with("Convert this chart"));
        (200, chat_ok(&json!({
            "chart_type": "bar", "title": "Revenue", "x_axis": "Year", "y_axis": "EUR m",
            "table_html": "<table><tr><th>Year</th><th>Revenue</th></tr><tr><td>2024</td><td>10~</td></tr></table>",
            "description": "Revenue grows year over year."
        }).to_string()))
    }))
    .await;
    let c = client(&registry_toml("vlm", "generic_json", &[&srv.base_url], ""));
    let png: Arc<[u8]> = page_png().bytes.clone();
    let crops = vec![
        FigureCrop { region: region(BlockKind::Figure, (0.0, 0.0, 0.5, 1.0), 0), uri: "s3://f1".into(), bytes: png.clone() },
        FigureCrop { region: region(BlockKind::Chart, (0.5, 0.0, 1.0, 1.0), 1), uri: "s3://c1".into(), bytes: png },
    ];
    let out = ChartToTable { client: c }.run(&ctx(), &[&Artifact::FigureCrops(crops)], &json!({"model": "vlm"})).await.unwrap();
    let Artifact::Blocks(blocks) = out.artifact else { panic!() };
    assert_eq!(srv.hits(), 1, "figure crop skipped");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Chart);
    let BlockContent::Chart { crop_uri, data_html, description } = &blocks[0].content else { panic!() };
    assert_eq!(crop_uri, "s3://c1");
    assert!(data_html.as_deref().unwrap().starts_with("<table>"));
    assert_eq!(description.as_deref(), Some("Revenue grows year over year."));
    assert_eq!(blocks[0].text, "Revenue\nRevenue grows year over year.\nYear\tRevenue\n2024\t10~");
    assert!(out.prompt_ref.unwrap().starts_with("chart_to_table@"));
}

#[tokio::test]
async fn page_parse_e2e_generic_json_with_schema_and_dots_ocr_native() {
    let srv = MockServer::start(Arc::new(|_, c| {
        if c.body.get("response_format").is_some() {
            (200, chat_ok(r#"{"blocks":[{"label":"title","bbox":[0,0,200,20],"reading_order":0,"score":0.9,"text":"Hello","level":1}]}"#))
        } else {
            assert!(c.user_text().starts_with("Please output the layout information"));
            (200, chat_ok(r#"[{"bbox":[0,0,200,20],"category":"Title","text":"Hello dots"}]"#))
        }
    }))
    .await;
    let toml = format!(
        "{}{}",
        registry_toml("vlm", "generic_json", &[&srv.base_url], ""),
        registry_toml("dots", "dots_ocr", &[&srv.base_url], "")
    );
    let c = client(&toml);
    let step = PageParseE2e { client: c };
    let out = step.run(&ctx(), &[&Artifact::PageImage(page_png())], &json!({"model": "vlm"})).await.unwrap();
    let Artifact::Blocks(b) = out.artifact else { panic!() };
    assert_eq!((b[0].kind, b[0].text.as_str(), b[0].level), (BlockKind::Title, "Hello", Some(1)));
    assert!((b[0].bbox.y1 - 0.2).abs() < 1e-6);
    let out = step.run(&ctx(), &[&Artifact::PageImage(page_png())], &json!({"model": "dots"})).await.unwrap();
    let Artifact::Blocks(b) = out.artifact else { panic!() };
    assert_eq!(b[0].text, "Hello dots");
    assert!(out.prompt_ref.unwrap().starts_with("dots_ocr_layout_all@"));
}

#[tokio::test]
async fn register_steps_covers_the_catalogue() {
    let c = client(&registry_toml("vlm", "generic_json", &["http://127.0.0.1:1/v1"], ""));
    let mut reg = StepRegistry::new();
    vs_models::register_steps(&mut reg, c);
    let mut kinds: Vec<&str> = reg.kinds().copied().collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        vec!["chart.to_table", "figure.describe", "formula.recognize", "layout.detect", "page.parse_e2e", "region.ocr", "table.recognize"]
    );
    let _: Value = json!(null);
}
