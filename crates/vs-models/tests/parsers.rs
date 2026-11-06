//! Parser tests against captured-style fixtures in `tests/fixtures/<family>/`.

use std::sync::Arc;

use vs_core::{BlockContent, BlockKind};
use vs_dag::{BlockDraft, PageImage};
use vs_models::parsers::{parser_for, FAMILIES};

fn page(w: u32, h: u32) -> PageImage {
    PageImage { uri: "mem://page".into(), bytes: Arc::from(vec![0u8; 4]), width_px: w, height_px: h, dpi: 150 }
}

fn parse(family: &str, raw: &str, size: (u32, u32)) -> Vec<BlockDraft> {
    let p = parser_for(family).unwrap();
    assert_eq!(p.family(), family);
    p.parse(raw, &page(size.0, size.1), size, "node-x").unwrap()
}

fn kinds(b: &[BlockDraft]) -> Vec<BlockKind> {
    b.iter().map(|x| x.kind).collect()
}

fn assert_sane(blocks: &[BlockDraft]) {
    for (i, b) in blocks.iter().enumerate() {
        assert_eq!(b.reading_order, i as u32, "reading order is dense and ordered");
        assert_eq!(b.node_id, "node-x");
        for v in [b.bbox.x0, b.bbox.y0, b.bbox.x1, b.bbox.y1] {
            assert!((0.0..=1.0).contains(&v), "bbox in [0,1]: {:?}", b.bbox);
        }
        assert!(b.bbox.x0 <= b.bbox.x1 && b.bbox.y0 <= b.bbox.y1);
        assert!((0.0..=1.0).contains(&b.confidence));
        match &b.content {
            BlockContent::Table { html } => assert!(html.to_ascii_lowercase().contains("<table"), "table html: {html}"),
            BlockContent::Formula { latex } => assert!(!latex.contains("$$"), "delimiters stripped: {latex}"),
            BlockContent::Text { text } => assert_eq!(text, &b.text),
            _ => {}
        }
    }
}

#[test]
fn every_family_has_a_parser() {
    for f in FAMILIES {
        parser_for(f).unwrap();
    }
    assert!(parser_for("nope").is_err());
}

#[test]
fn paddleocr_vl_parsing_res_list() {
    let raw = include_str!("fixtures/paddleocr_vl/page_parsing_res_list.txt");
    let b = parse("paddleocr_vl", raw, (1240, 1754));
    assert_sane(&b);
    assert_eq!(b.len(), 11, "nothing dropped");
    // block_order wins over array order: table_title (order 3) precedes table (order 4).
    assert_eq!(
        kinds(&b),
        vec![
            BlockKind::Title,
            BlockKind::Paragraph,
            BlockKind::Heading,
            BlockKind::Caption,
            BlockKind::Table,
            BlockKind::Formula,
            BlockKind::Other,
            BlockKind::Figure,
            BlockKind::Caption,
            BlockKind::Footnote,
            BlockKind::PageNumber,
        ]
    );
    assert_eq!(b[0].text, "Attention Is All You Need");
    assert!((b[0].bbox.x0 - 112.0 / 1240.0).abs() < 1e-5);
    assert!((b[0].bbox.y1 - 152.0 / 1754.0).abs() < 1e-5);
    assert_eq!(b[2].level, Some(2));
    let BlockContent::Table { html } = &b[4].content else { panic!("table") };
    assert!(html.contains("<th>Model</th>"));
    assert_eq!(b[4].text, "Model\tBLEU\nTransformer (base)\t27.3\nTransformer (big)\t28.4");
    let BlockContent::Formula { latex } = &b[5].content else { panic!("formula") };
    assert!(latex.starts_with("\\mathrm{Attention}"));
    assert_eq!(b[6].text, "(1)"); // formula_number kept as Other
    assert!(matches!(b[7].content, BlockContent::Figure { .. }));
}

#[test]
fn paddleocr_vl_markdown_fallback() {
    let raw = include_str!("fixtures/paddleocr_vl/region_markdown.txt");
    let b = parse("paddleocr_vl", raw, (800, 600));
    assert_sane(&b);
    assert_eq!(kinds(&b), vec![BlockKind::Title, BlockKind::Paragraph, BlockKind::Paragraph]);
    assert_eq!(b[0].text, "Experimental Setup");
    assert_eq!(b[0].bbox.x1, 1.0, "fallback blocks span the page");
    assert!(b[2].text.starts_with("Sentences were encoded"));
}

#[test]
fn mineru_content_list() {
    let raw = include_str!("fixtures/mineru/content_list.txt");
    let b = parse("mineru", raw, (1000, 1414));
    assert_sane(&b);
    assert_eq!(
        kinds(&b),
        vec![
            BlockKind::Heading,   // text_level 1
            BlockKind::Paragraph,
            BlockKind::Heading,   // text_level 2
            BlockKind::Figure,
            BlockKind::Caption,   // image_caption
            BlockKind::Table,
            BlockKind::Caption,   // table_caption
            BlockKind::Footnote,  // table_footnote
            BlockKind::Formula,
            BlockKind::List,
            BlockKind::Reference,
            BlockKind::Other,     // discarded is kept
            BlockKind::PageNumber,
        ]
    );
    assert_eq!((b[0].level, b[2].level), (Some(1), Some(2)));
    // bbox in thousandths regardless of the model input size.
    assert!((b[0].bbox.x0 - 0.102).abs() < 1e-5 && (b[0].bbox.y1 - 0.118).abs() < 1e-5);
    assert_eq!(b[4].text, "Figure 1. Training error (left) and test error (right) on CIFAR-10.");
    let BlockContent::Table { html } = &b[5].content else { panic!("table") };
    assert!(html.starts_with("<table>"));
    assert_eq!(b[7].text, "FLOPs are counted for the 224x224 input.");
    let BlockContent::Formula { latex } = &b[8].content else { panic!("formula") };
    assert_eq!(latex, "\\mathcal{F}(\\mathbf{x}) := \\mathcal{H}(\\mathbf{x}) - \\mathbf{x}");
    assert_eq!(b[9].text, "Identity shortcuts add no parameter.\nProjection shortcuts are used when dimensions increase.");
    assert!(b[11].text.starts_with("arXiv:1512.03385"));
}

#[test]
fn mineru_vlm_tagged_output() {
    let raw = include_str!("fixtures/mineru/vlm_tagged.txt");
    let b = parse("mineru", raw, (1000, 1414));
    assert_sane(&b);
    assert_eq!(
        kinds(&b),
        vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Table, BlockKind::Formula, BlockKind::Figure, BlockKind::Caption]
    );
    assert_eq!(b[0].text, "# Deep Residual Learning for Image Recognition");
    assert_eq!(b[0].level, Some(1));
    assert!((b[2].bbox.y0 - 0.65).abs() < 1e-5);
    assert!(matches!(&b[2].content, BlockContent::Table { html } if html.contains("conv1")));
    assert!(matches!(&b[3].content, BlockContent::Formula { latex } if latex.starts_with("\\mathcal{F}")));
    // Layout-only block (no <|md_start|>) still appears, with empty content.
    assert_eq!(b[4].text, "");
    assert_eq!(b[5].text, "Figure 1. Training error on CIFAR-10.");
}

#[test]
fn dots_ocr_page() {
    let raw = include_str!("fixtures/dots_ocr/page.txt");
    let b = parse("dots_ocr", raw, (1024, 1280));
    assert_sane(&b);
    assert_eq!(b.len(), 13);
    assert_eq!(
        kinds(&b),
        vec![
            BlockKind::Title,
            BlockKind::Header,
            BlockKind::Heading,
            BlockKind::Paragraph,
            BlockKind::Caption,
            BlockKind::Table,
            BlockKind::List,
            BlockKind::List,
            BlockKind::Formula,
            BlockKind::Figure,
            BlockKind::Caption,
            BlockKind::Footnote,
            BlockKind::Footer,
        ]
    );
    assert!((b[0].bbox.x0 - 88.0 / 1024.0).abs() < 1e-5);
    assert!((b[0].bbox.y1 - 110.0 / 1280.0).abs() < 1e-5);
    assert_eq!(b[5].text, "Segment\tQ2\tQ3\nProducts\t41.2\t42.0\nServices\t30.1\t37.9");
    assert!(matches!(&b[8].content, BlockContent::Formula { latex } if latex.starts_with("\\text{margin}")));
    // Picture without text is kept as an empty figure block.
    assert_eq!(b[9].text, "");
    assert!(matches!(b[9].content, BlockContent::Figure { description: None, .. }));
}

#[test]
fn dots_ocr_pixel_bboxes_are_clamped_to_model_input() {
    let raw = r#"[{"bbox":[0,0,2048,10],"category":"Text","text":"wide"}]"#;
    let b = parse("dots_ocr", raw, (1024, 1024));
    assert_eq!(b[0].bbox.x1, 1.0);
}

#[test]
fn logics_parsing_html() {
    let raw = include_str!("fixtures/logics_parsing/page.txt");
    let b = parse("logics_parsing", raw, (1000, 1300));
    assert_sane(&b);
    assert_eq!(
        kinds(&b),
        vec![
            BlockKind::Title,
            BlockKind::Header,
            BlockKind::Paragraph,
            BlockKind::Heading, // class title + <h2> → heading level 2
            BlockKind::Table,
            BlockKind::Caption,
            BlockKind::Formula,
            BlockKind::Formula, // chemistry (SMILES) kept as formula content
            BlockKind::Figure,
            BlockKind::Caption,
            BlockKind::Code,
            BlockKind::Footnote,
            BlockKind::PageNumber,
        ]
    );
    assert_eq!(b[0].text, "Synthesis of Novel Heterocyclic Compounds");
    assert!((b[0].bbox.x0 - 0.11).abs() < 1e-5 && (b[0].bbox.y1 - 0.12).abs() < 1e-5);
    assert!(b[2].text.contains("62 %"), "nbsp decoded: {}", b[2].text);
    assert_eq!((b[3].text.as_str(), b[3].level), ("2. Results and Discussion", Some(2)));
    let BlockContent::Table { html } = &b[4].content else { panic!("table") };
    assert!(html.starts_with("<table>") && html.contains("<sub>2</sub>"));
    assert_eq!(b[4].text, "Entry\tCatalyst\tYield (%)\n1\tPd(OAc)2\t71\n2\tCuI\t48");
    assert_eq!(b[6].content, BlockContent::Formula { latex: "k = A e^{-E_a / RT}".into() });
    assert_eq!(b[7].text, "CC(=O)Oc1ccccc1C(=O)O");
    assert_eq!(b[8].text, "Reaction scheme"); // img alt
    assert!(b[10].text.starts_with("for step in range(3):"));
    assert_eq!(b[11].text, "† Corresponding author.");
}

#[test]
fn logics_parsing_without_bboxes_falls_back() {
    let b = parse("logics_parsing", "<p>Just a paragraph.</p><p>And another.</p>", (1000, 1000));
    assert_sane(&b);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].text, "Just a paragraph.\nAnd another.");
}

#[test]
fn generic_json_blocks() {
    let raw = include_str!("fixtures/generic_json/blocks.txt");
    let b = parse("generic_json", raw, (1240, 1754));
    assert_sane(&b);
    assert_eq!(
        kinds(&b),
        vec![
            BlockKind::Title,
            BlockKind::Paragraph,
            BlockKind::Table,
            BlockKind::Formula,
            BlockKind::Heading,
            BlockKind::Stamp,
            BlockKind::Other, // "watermark" is unknown but kept
        ]
    );
    assert!((b[0].bbox.x0 - 0.08).abs() < 1e-6, "normalised bbox passes through");
    assert!((b[0].confidence - 0.97).abs() < 1e-6);
    assert!(matches!(&b[2].content, BlockContent::Table { html } if html.contains("<th>Item</th>")));
    assert_eq!(b[2].text, "Item\tQty\tPrice\nWidget\t3\t29.90");
    assert_eq!(b[3].content, BlockContent::Formula { latex: "\\sum_i q_i p_i".into() });
    assert_eq!(b[4].level, Some(2));
    assert_eq!(b[6].text, "DRAFT");
}

#[test]
fn generic_json_pixels_and_reading_order() {
    let raw = include_str!("fixtures/generic_json/pixel_bboxes_reordered.txt");
    let b = parse("generic_json", raw, (1000, 1000));
    assert_sane(&b);
    assert_eq!(b.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), vec!["first", "second"]);
    assert!((b[0].bbox.y0 - 0.1).abs() < 1e-6);
}

#[test]
fn generic_json_requires_a_list() {
    let p = parser_for("generic_json").unwrap();
    let err = p.parse(r#"{"foo": 1}"#, &page(10, 10), (10, 10), "n").unwrap_err();
    assert!(err.to_string().contains("generic_json"));
}
