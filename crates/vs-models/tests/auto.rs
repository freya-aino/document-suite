//! The `auto` family: routing output from a model we have no parser for.
//!
//! These run against the same captured fixtures the per-family parsers use, so they prove
//! `auto` routes *real* model output to the right parser rather than a toy sample shaped
//! to match the dispatch code.

use std::sync::Arc;

use vs_core::BlockKind;
use vs_dag::PageImage;
use vs_models::parsers::{coord_scale_from_name, parser_for, Auto, AutoPath, CoordScale};

fn page(w: u32, h: u32) -> PageImage {
    PageImage { uri: "mem://page".into(), bytes: Arc::from(vec![0u8; 4]), width_px: w, height_px: h, dpi: 150 }
}

fn fixture(family: &str, name: &str) -> String {
    let path = format!("{}/tests/fixtures/{family}/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn route(raw: &str, size: (u32, u32)) -> AutoPath {
    Auto::default().parse_reporting(raw, &page(size.0, size.1), size, "node-x").1
}

#[test]
fn routes_each_family_fixture_to_its_own_parser() {
    // Every one of these is output `auto` has never been told the shape of.
    for (family, name, size) in [
        ("mineru", "vlm_tagged.txt", (1000, 1400)),
        ("mineru", "content_list.txt", (1000, 1400)),
        ("dots_ocr", "page.txt", (1000, 1400)),
        ("logics_parsing", "page.txt", (1000, 1400)),
        ("generic_json", "blocks.txt", (1000, 1400)),
    ] {
        let raw = fixture(family, name);
        let path = route(&raw, size);
        assert_ne!(path, AutoPath::Markdown, "{family}/{name} fell through to Markdown");
        // `content_list.txt` is MinerU's JSON form. Our own schema parser *can* read it
        // well enough to look successful, which is exactly why candidate ordering is by
        // shape evidence: it must still reach the parser that knows its bboxes are
        // per-mille, not the one that would read them as pixels.
        assert_eq!(path, AutoPath::Family(family), "{family}/{name} routed to {path:?}");
    }
}

#[test]
fn produces_the_same_blocks_as_the_named_family() {
    // Routing is only useful if the result is identical to naming the family outright.
    for (family, name, size) in [
        ("mineru", "vlm_tagged.txt", (1000, 1400)),
        ("dots_ocr", "page.txt", (1000, 1400)),
        ("logics_parsing", "page.txt", (1000, 1400)),
    ] {
        let raw = fixture(family, name);
        let p = page(size.0, size.1);
        let direct = parser_for(family).unwrap().parse(&raw, &p, size, "node-x").unwrap();
        let auto = Auto::default().parse_reporting(&raw, &p, size, "node-x").0;
        assert_eq!(auto.len(), direct.len(), "{family}/{name} block count");
        for (a, d) in auto.iter().zip(&direct) {
            assert_eq!((a.kind, &a.text), (d.kind, &d.text), "{family}/{name}");
            assert_eq!(a.bbox, d.bbox, "{family}/{name} bbox");
        }
    }
}

#[test]
fn prose_falls_back_to_markdown() {
    let raw = "# Results\n\nThe model reaches 91.2 F1 on the test split.\n\n- first\n- second";
    let (blocks, path) = Auto::default().parse_reporting(raw, &page(800, 1000), (800, 1000), "node-x");
    assert_eq!(path, AutoPath::Markdown);
    assert!(!blocks.is_empty(), "the Markdown splitter must never lose content");
    assert!(
        blocks.iter().any(|b| matches!(b.kind, BlockKind::Heading | BlockKind::Title)),
        "the `# Results` heading must survive: {:?}",
        blocks.iter().map(|b| b.kind).collect::<Vec<_>>()
    );
    assert!(blocks.iter().any(|b| b.kind == BlockKind::List), "the bullet list must survive");
}

/// The guard that makes the ordering correct: MinerU's tagged output is not JSON, so if
/// `generic_json` were tried first it would swallow it into a Markdown fallback.
#[test]
fn tagged_output_is_not_swallowed_by_the_generic_schema() {
    let raw = fixture("mineru", "vlm_tagged.txt");
    assert!(Auto::candidates(&raw).first() == Some(&"mineru"), "candidates: {:?}", Auto::candidates(&raw));
    assert_eq!(route(&raw, (1000, 1400)), AutoPath::Family("mineru"));
}

/// JSON that our schema cannot read must fall through to the family that can, not fail.
#[test]
fn unreadable_json_falls_through_to_the_next_family() {
    let raw = fixture("paddleocr_vl", "page_parsing_res_list.txt");
    let candidates = Auto::candidates(&raw);
    assert!(candidates.contains(&"generic_json"), "JSON should offer our schema first: {candidates:?}");
    assert!(candidates.contains(&"paddleocr_vl"));
    let path = route(&raw, (1000, 1400));
    assert_eq!(path, AutoPath::Family("paddleocr_vl"), "routed to {path:?}");
}

#[test]
fn empty_output_still_yields_something() {
    // A model that returns nothing must not panic or error; it yields no blocks via the
    // Markdown path, and the step's own `optional` flag decides what that means.
    let (blocks, path) = Auto::default().parse_reporting("", &page(800, 1000), (800, 1000), "node-x");
    assert_eq!(path, AutoPath::Markdown);
    assert!(blocks.is_empty());
}

#[test]
fn declared_coord_scale_is_honoured() {
    // Thousandth coordinates on a 768 px page: read as pixels, 900 clamps to the edge.
    let raw = r#"{"blocks":[{"kind":"paragraph","bbox":[100,200,900,400],"text":"x"}]}"#;
    let size = (768, 768);
    let p = page(size.0, size.1);

    let as_pixels = Auto::with_scale(Some(CoordScale::Pixels)).parse_reporting(raw, &p, size, "n").0;
    let as_thousandths = Auto::with_scale(Some(CoordScale::Thousandths)).parse_reporting(raw, &p, size, "n").0;

    assert_eq!(as_pixels[0].bbox.x1, 1.0, "900 px on a 768 px page clamps to the edge");
    assert!(
        (as_thousandths[0].bbox.x1 - 0.9).abs() < 1e-5,
        "900 per mille is 0.9 of the page, got {}",
        as_thousandths[0].bbox.x1
    );
}

#[test]
fn coord_scale_names() {
    assert_eq!(coord_scale_from_name("thousandths"), Some(CoordScale::Thousandths));
    assert_eq!(coord_scale_from_name("  Per_Mille "), Some(CoordScale::Thousandths));
    assert_eq!(coord_scale_from_name("pixels"), Some(CoordScale::Pixels));
    assert_eq!(coord_scale_from_name("fractions"), Some(CoordScale::Pixels));
    // A typo must not stop a worker starting; detection takes over.
    assert_eq!(coord_scale_from_name("thousanths"), None);
}

/// `auto` must be in the catalogue, or DAG-list validation would reject a DAG naming it.
#[test]
fn auto_is_a_registered_family() {
    assert!(vs_models::parsers::FAMILIES.contains(&"auto"));
    assert_eq!(parser_for("auto").unwrap().family(), "auto");
}

/// `auto` must *read* grounded-tag output, not split it as prose. This is the case the
/// family was added for: before it, a model in this shape reached the Markdown splitter
/// and every bounding box on the page was lost.
#[test]
fn auto_reads_grounded_tag_output_structurally() {
    let raw = include_str!("fixtures/grounded_tags/page.txt");
    let auto = Auto::default();
    let (blocks, path) = auto.parse_reporting(raw, &page(1000, 1000), (1000, 1000), "n");
    assert_eq!(path, AutoPath::Family("grounded_tags"), "auto fell back instead of reading the tags");
    assert!(blocks.len() >= 5);
    // Boxes, not one paragraph per line: the difference the family makes.
    assert!(blocks.iter().any(|b| b.bbox.x0 > 0.0 && b.bbox.y1 < 1.0));
}

/// And MinerU's tagged output must still go to MinerU: the two conventions share the
/// `<|ref_start|>` spelling, so whichever parser finds its own box tag has to win.
#[test]
fn minerus_tags_are_not_mistaken_for_the_grounded_convention() {
    let raw = "<|ref_start|>text<|ref_end|><|box_start|>100 200 900 300<|box_end|><|md_start|>Hello<|md_end|>";
    let auto = Auto::default();
    let (blocks, path) = auto.parse_reporting(raw, &page(1000, 1000), (1000, 1000), "n");
    assert_eq!(path, AutoPath::Family("mineru"), "got {path:?}");
    assert_eq!(blocks.len(), 1);
}
