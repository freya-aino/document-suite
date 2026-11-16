//! Nodes for things *inside* a block: table cells and text found in images.
//!
//! Until now a table reached the graph as one opaque HTML string and a figure as one
//! description, so neither could be cited at the level anyone actually cares about. A
//! question answered by one number in a results table could only be traced to "that
//! table", and a label inside a chart was unreachable even when the same term appeared
//! in the body text.

use serde_json::{json, Value};

use vs_core::{BlockContent, BlockKind};
use vs_extract::tables::parse_html_table;

use crate::{DocGraph, EdgeKind, NodeKind};

/// Longest in-image text promoted to a node. Beyond this the model has transcribed a
/// screenshot of a document rather than labelled a figure, and the body text is the
/// better source for all of it.
const MAX_IMAGE_TEXT: usize = 2000;

/// Shortest in-image fragment worth matching. One- and two-character fragments are axis
/// ticks and legend letters; matching them to body text produces noise, not links.
const MIN_FRAGMENT_CHARS: usize = 3;

/// A purely numeric run of several tokens is an axis scale, not a label: matching
/// "1 2 3" by containment hits any text that happens to contain that sequence. A single
/// numeric token of this length or more is kept, because that is the shape of the
/// identifiers worth finding — a part number or an invoice reference printed on a figure.
const MIN_NUMERIC_TOKEN_CHARS: usize = 4;

/// Whether a fragment carries enough to be worth matching against body text.
fn is_matchable(f: &str) -> bool {
    if f.chars().count() < MIN_FRAGMENT_CHARS {
        return false;
    }
    if f.chars().any(char::is_alphabetic) {
        return true;
    }
    // No letters: keep it only as one token of some length.
    !f.contains(char::is_whitespace) && f.chars().count() >= MIN_NUMERIC_TOKEN_CHARS
}

/// Table cell key: stable, and derived from the table it belongs to, so a cell can be
/// cited without a lookup table.
pub fn cell_key(block_hk: &str, row: usize, col: usize) -> String {
    format!("cell:{block_hk}:{row}:{col}")
}

/// In-image text key.
pub fn image_text_key(block_hk: &str, index: usize) -> String {
    format!("imgtext:{block_hk}:{index}")
}

/// Splits in-image text into fragments worth matching separately.
///
/// The model returns one string of "all legible text in reading order", which for a chart
/// is a run of axis labels, legend entries and a title. Lines and the obvious separators
/// are the only structure available, and it is enough: a legend entry is what matches a
/// term in the body text, not the whole run.
pub fn fragments(text: &str) -> Vec<String> {
    text.split(['\n', '|', ';', '\t'])
        .flat_map(|part| part.split("  "))
        .map(|f| f.trim().trim_matches(|c: char| !c.is_alphanumeric()).trim().to_string())
        .filter(|f| is_matchable(f))
        .collect()
}

/// `text_in_image` from a figure or chart description, if the step stored structured
/// output. A plain-prose description is not JSON and yields nothing, which is correct —
/// there is no in-image text to speak of in that case.
pub fn text_in_image(content: &BlockContent) -> Option<String> {
    let description = match content {
        BlockContent::Figure { description, .. } | BlockContent::Chart { description, .. } => description.as_deref()?,
        _ => return None,
    };
    let v: Value = serde_json::from_str(description.trim()).ok()?;
    let text = v.get("text_in_image")?.as_str()?.trim();
    if text.is_empty() || text.len() > MAX_IMAGE_TEXT {
        return None;
    }
    Some(text.to_string())
}

/// Adds cell nodes for every table, and `has_cell` edges from the table block.
///
/// Row and column indices are the parsed grid's own, so a reader can address a cell by
/// position. `column_header` carries the header-row value for that column where there is
/// one, which is what makes a cell self-describing: `cell:…:3:2` means nothing on its
/// own, but the same cell carrying `column_header: "F1"` is citable evidence.
pub fn add_table_cells(g: &mut DocGraph, block_hk: &str, content: &BlockContent) -> usize {
    let html = match content {
        BlockContent::Table { html } => html.as_str(),
        BlockContent::Chart { data_html: Some(html), .. } => html.as_str(),
        _ => return 0,
    };
    let rows = parse_html_table(html);
    if rows.is_empty() {
        return 0;
    }
    // Column names come from the first header row, if the table has one.
    let headers: Vec<String> = rows.iter().find(|r| r.header).map(|r| r.cells.clone()).unwrap_or_default();
    let mut added = 0;
    for (r, row) in rows.iter().enumerate() {
        for (c, text) in row.cells.iter().enumerate() {
            if text.trim().is_empty() {
                continue;
            }
            let key = cell_key(block_hk, r, c);
            g.push_node(
                NodeKind::Cell,
                &key,
                json!({
                    "row": r,
                    "col": c,
                    "header": row.header,
                    "text": text,
                    "column_header": headers.get(c).filter(|h| !h.trim().is_empty()),
                }),
            );
            g.push_edge(EdgeKind::HasCell, block_hk, &key, Value::Null);
            added += 1;
        }
    }
    added
}

/// Adds in-image text nodes for a figure or chart, and links each fragment to body
/// blocks and entities whose text contains it.
///
/// Matching is case-insensitive containment on fragments of at least three characters.
/// Deliberately conservative: a looser match (stemming, fuzzy) on three-character
/// fragments would connect a chart's "AUC" label to every occurrence of "auction", and a
/// wrong edge in a provenance graph is worse than a missing one.
pub fn add_image_text(
    g: &mut DocGraph,
    block_hk: &str,
    content: &BlockContent,
    body: &[(String, String)],
    entities: &[(String, String)],
) -> usize {
    let Some(text) = text_in_image(content) else { return 0 };
    let mut added = 0;
    for (i, fragment) in fragments(&text).into_iter().enumerate() {
        let key = image_text_key(block_hk, i);
        g.push_node(NodeKind::ImageText, &key, json!({ "text": fragment, "index": i }));
        g.push_edge(EdgeKind::TextInImage, block_hk, &key, Value::Null);
        added += 1;

        let needle = fragment.to_lowercase();
        // The figure's own block is excluded: its `text` is the description plus this
        // very string, so every fragment would match it.
        for (target, haystack) in body.iter().filter(|(k, _)| k != block_hk) {
            if haystack.contains(&needle) {
                g.push_edge(EdgeKind::MatchesText, &key, target, json!({ "via": "block_text" }));
            }
        }
        for (target, name) in entities {
            if name.contains(&needle) || needle.contains(name.as_str()) {
                g.push_edge(EdgeKind::MatchesText, &key, target, json!({ "via": "entity_name" }));
            }
        }
    }
    added
}

/// Block kinds whose content can hold in-image text.
pub fn is_image_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Figure | BlockKind::Chart)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = "<table><tr><th>Model</th><th>F1</th></tr>\
                        <tr><td>BERT</td><td>88.5</td></tr>\
                        <tr><td>RoBERTa</td><td></td></tr></table>";

    fn graph() -> DocGraph {
        DocGraph::default()
    }

    #[test]
    fn cells_get_position_and_column_header() {
        let mut g = graph();
        let added = add_table_cells(&mut g, "tbl", &BlockContent::Table { html: HTML.into() });
        // Six grid positions, but one cell is empty and contributes nothing.
        assert_eq!(added, 5);
        let cell = g.nodes.iter().find(|n| n.key == cell_key("tbl", 1, 1)).expect("row 1 col 1");
        assert_eq!(cell.props["text"], "88.5");
        assert_eq!(cell.props["column_header"], "F1", "a cell must be self-describing");
        assert_eq!(cell.props["header"], false);
        let header = g.nodes.iter().find(|n| n.key == cell_key("tbl", 0, 0)).unwrap();
        assert_eq!(header.props["header"], true);
        // Every cell hangs off its table.
        assert_eq!(g.edges.iter().filter(|e| e.kind == EdgeKind::HasCell && e.from == "tbl").count(), 5);
    }

    #[test]
    fn a_chart_data_table_also_yields_cells() {
        let mut g = graph();
        let content = BlockContent::Chart {
            crop_uri: "crop://x".into(),
            data_html: Some(HTML.into()),
            description: None,
        };
        assert_eq!(add_table_cells(&mut g, "ch", &content), 5);
    }

    #[test]
    fn non_tables_and_empty_tables_add_nothing() {
        let mut g = graph();
        assert_eq!(add_table_cells(&mut g, "b", &BlockContent::Text { text: "x".into() }), 0);
        assert_eq!(add_table_cells(&mut g, "b", &BlockContent::Table { html: "<table></table>".into() }), 0);
        assert_eq!(add_table_cells(&mut g, "b", &BlockContent::Empty), 0);
        assert!(g.nodes.is_empty() && g.edges.is_empty());
    }

    #[test]
    fn in_image_text_comes_only_from_structured_output() {
        let structured = BlockContent::Figure {
            crop_uri: "c".into(),
            description: Some(json!({ "description": "a chart", "text_in_image": "Accuracy\nEpochs" }).to_string()),
        };
        assert_eq!(text_in_image(&structured).as_deref(), Some("Accuracy\nEpochs"));
        // Plain prose is not JSON, so there is no in-image text to take.
        let prose = BlockContent::Figure { crop_uri: "c".into(), description: Some("A bar chart.".into()) };
        assert_eq!(text_in_image(&prose), None);
        let none = BlockContent::Figure { crop_uri: "c".into(), description: None };
        assert_eq!(text_in_image(&none), None);
        // An over-long transcription is the body text's job, not a figure label's.
        let huge = BlockContent::Figure {
            crop_uri: "c".into(),
            description: Some(json!({ "text_in_image": "x".repeat(MAX_IMAGE_TEXT + 1) }).to_string()),
        };
        assert_eq!(text_in_image(&huge), None);
    }

    #[test]
    fn fragments_split_on_the_structure_that_exists() {
        assert_eq!(fragments("Accuracy\nEpochs | Loss"), vec!["Accuracy", "Epochs", "Loss"]);
        assert_eq!(fragments("Fig 1:  BERT  RoBERTa"), vec!["Fig 1", "BERT", "RoBERTa"]);
        // Axis ticks and legend letters carry no selectivity.
        assert!(fragments("1 2 3\na\nb").is_empty());
        assert!(fragments("").is_empty());
        // A single numeric token of some length is the shape of an identifier, which is
        // exactly what is worth finding on a figure.
        assert_eq!(fragments("2026-0042"), vec!["2026-0042"]);
        assert!(fragments("0 5 10 15 20").is_empty(), "an axis scale is not a label");
    }

    #[test]
    fn fragments_link_to_matching_blocks_and_entities() {
        let mut g = graph();
        let content = BlockContent::Chart {
            crop_uri: "c".into(),
            data_html: None,
            description: Some(json!({ "text_in_image": "RoBERTa\nGLUE score" }).to_string()),
        };
        let body = vec![
            ("b1".to_string(), "we compare roberta against the baseline".to_string()),
            ("b2".to_string(), "unrelated prose about something else".to_string()),
            // The figure's own block: its text always contains the fragment.
            ("fig".to_string(), "roberta glue score".to_string()),
        ];
        let entities = vec![("e-roberta".to_string(), "roberta".to_string())];
        assert_eq!(add_image_text(&mut g, "fig", &content, &body, &entities), 2);

        let frag = image_text_key("fig", 0);
        let matches: Vec<&str> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::MatchesText && e.from == frag)
            .map(|e| e.to.as_str())
            .collect();
        assert!(matches.contains(&"b1"), "{matches:?}");
        assert!(matches.contains(&"e-roberta"), "{matches:?}");
        assert!(!matches.contains(&"b2"));
        assert!(!matches.contains(&"fig"), "a figure must not match its own description");
        // Each fragment hangs off the image block.
        assert_eq!(g.edges.iter().filter(|e| e.kind == EdgeKind::TextInImage).count(), 2);
    }

    #[test]
    fn image_kinds() {
        assert!(is_image_kind(BlockKind::Figure) && is_image_kind(BlockKind::Chart));
        assert!(!is_image_kind(BlockKind::Table) && !is_image_kind(BlockKind::Paragraph));
    }
}
