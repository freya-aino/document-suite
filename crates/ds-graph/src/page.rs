//! Nodes for things *inside* a block: table cells and text found in images.
//!
//! Until now a table reached the graph as one opaque HTML string and a figure as one
//! description, so neither could be cited at the level anyone actually cares about. A
//! question answered by one number in a results table could only be traced to "that
//! table", and a label inside a chart was unreachable even when the same term appeared
//! in the body text.

use std::collections::HashMap;

use serde_json::{json, Value};

use ds_core::{BlockContent, BlockKind};
use ds_extract::tables::parse_html_table;

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

/// Whether a string carries enough to be worth matching.
///
/// Applied to both sides of a match, not just the fragment. An entity named "IT" or "AI"
/// is below this bar, which is the point: before the entity side was checked, the entity
/// "IT" matched every fragment containing those two letters in that order — "Monitoring"
/// among them — and an entity with an empty name matched *every* fragment, because every
/// string contains the empty string.
pub fn is_matchable(f: &str) -> bool {
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
    if text.is_empty() || text.chars().count() > MAX_IMAGE_TEXT {
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

/// Alphanumeric runs of a folded string. Everything else is a boundary.
fn tokens(folded: &str) -> impl Iterator<Item = &str> {
    folded.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty())
}

/// Whether `needle` occurs in `haystack` with a word boundary at both ends.
///
/// Plain `str::contains` is what let the entity "IT" be found inside "monitoring". A
/// match is only a match when neither end of it runs into more of a word; "roberta" in
/// "we compare roberta against" is one, "roberta" in "robertazzi" is not.
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let open = haystack[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let close = haystack[end..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if open && close {
            return true;
        }
        // Step one character past this occurrence and keep looking: the same needle can
        // appear again, on a boundary this time.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// What a fragment of in-image text can match: every body block's text and every
/// entity's name, folded once per document.
///
/// Folding lives in here rather than in the caller on purpose. The matcher used to
/// lowercase only the needle and trust callers to hand it lowercased haystacks, which
/// the one caller happened to do and the tests baked in — so a second caller would have
/// got silently case-sensitive matching with nothing failing.
///
/// The token index is the other reason this type exists. Matching used to be a full scan
/// of every body block for every fragment: on a 500-page document that is tens of
/// millions of substring searches over the whole document's text. A block can only
/// contain a fragment if it contains the fragment's rarest token, so the posting list of
/// that token is an exact candidate set, not an approximation.
#[derive(Default)]
pub struct MatchIndex {
    /// `(node key, folded text)` per body block.
    blocks: Vec<(String, String)>,
    /// Token → indices into `blocks`.
    by_token: HashMap<String, Vec<u32>>,
    /// `(node key, folded name)` per entity whose name clears `is_matchable`.
    entities: Vec<(String, String)>,
}

impl MatchIndex {
    /// Folds and indexes the match targets. `blocks` is `(block_hk, text)`, `entities`
    /// is `(entity key, name)`.
    pub fn build(
        blocks: impl IntoIterator<Item = (String, String)>,
        entities: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let mut idx = MatchIndex::default();
        for (key, text) in blocks {
            let folded = text.to_lowercase();
            let at = idx.blocks.len() as u32;
            for token in tokens(&folded) {
                let postings = idx.by_token.entry(token.to_string()).or_default();
                // One posting per block per token, however often the token occurs.
                if postings.last() != Some(&at) {
                    postings.push(at);
                }
            }
            idx.blocks.push((key, folded));
        }
        idx.entities = entities
            .into_iter()
            .map(|(key, name)| (key, name.to_lowercase()))
            .filter(|(_, name)| is_matchable(name))
            .collect();
        idx
    }

    /// Every target `fragment` matches, as `(node key, via)`. `exclude` is the figure's
    /// own block, whose text is the description plus this very string.
    fn matches(&self, fragment: &str, exclude: &str) -> Vec<(&str, &'static str)> {
        let folded = fragment.to_lowercase();
        let mut out = Vec::new();
        // The rarest token's postings: a block containing the whole fragment contains
        // every one of its tokens, so nothing outside that list can match.
        let rarest = tokens(&folded)
            .filter_map(|t| self.by_token.get(t))
            .min_by_key(|postings| postings.len());
        for &i in rarest.map(Vec::as_slice).unwrap_or_default() {
            let (key, text) = &self.blocks[i as usize];
            if key != exclude && contains_phrase(text, &folded) {
                out.push((key.as_str(), "block_text"));
            }
        }
        // Entities are few enough per document to scan, and need both directions: a
        // fragment can be an entity's name or contain it.
        for (key, name) in &self.entities {
            if contains_phrase(name, &folded) || contains_phrase(&folded, name) {
                out.push((key.as_str(), "entity_name"));
            }
        }
        out
    }
}

/// Adds in-image text nodes for a figure or chart, and links each fragment to body
/// blocks and entities whose text contains it.
///
/// Matching is case-insensitive containment on word boundaries, over fragments of at
/// least three characters, and both sides of a match must clear `is_matchable`.
/// Deliberately conservative: a looser match (stemming, fuzzy) on three-character
/// fragments would connect a chart's "AUC" label to every occurrence of "auction", and a
/// wrong edge in a provenance graph is worse than a missing one.
pub fn add_image_text(g: &mut DocGraph, block_hk: &str, content: &BlockContent, index: &MatchIndex) -> usize {
    let Some(text) = text_in_image(content) else { return 0 };
    let mut added = 0;
    for (i, fragment) in fragments(&text).into_iter().enumerate() {
        let key = image_text_key(block_hk, i);
        g.push_node(NodeKind::ImageText, &key, json!({ "text": fragment, "index": i }));
        g.push_edge(EdgeKind::TextInImage, block_hk, &key, Value::Null);
        added += 1;

        for (target, via) in index.matches(&fragment, block_hk) {
            g.push_edge(EdgeKind::MatchesText, &key, target, json!({ "via": via }));
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
    fn phrase_matching_respects_word_boundaries() {
        assert!(contains_phrase("we compare roberta against", "roberta"));
        assert!(contains_phrase("roberta", "roberta"));
        assert!(contains_phrase("(roberta)", "roberta"));
        // The bug this pins: "it" is in "monitoring", but not as a word.
        assert!(!contains_phrase("monitoring", "it"));
        assert!(!contains_phrase("robertazzi scores", "roberta"));
        assert!(!contains_phrase("anything", ""));
        // An earlier non-boundary occurrence must not hide a later real one.
        assert!(contains_phrase("robertazzi and roberta", "roberta"));
        // Multi-byte characters either side of the match.
        assert!(contains_phrase("über roberta — see", "roberta"));
    }

    #[test]
    fn fragments_link_to_matching_blocks_and_entities() {
        let mut g = graph();
        let content = BlockContent::Chart {
            crop_uri: "c".into(),
            data_html: None,
            description: Some(json!({ "text_in_image": "RoBERTa\nGLUE score" }).to_string()),
        };
        // Deliberately mixed case: folding is the index's job, not the caller's.
        let index = MatchIndex::build(
            vec![
                ("b1".to_string(), "We compare RoBERTa against the baseline".to_string()),
                ("b2".to_string(), "Unrelated prose about something else".to_string()),
                // The figure's own block: its text always contains the fragment.
                ("fig".to_string(), "RoBERTa GLUE score".to_string()),
            ],
            vec![("e-roberta".to_string(), "RoBERTa".to_string())],
        );
        assert_eq!(add_image_text(&mut g, "fig", &content, &index), 2);

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

    /// The three ways the old matcher produced edges that were simply wrong.
    #[test]
    fn short_and_empty_entity_names_match_nothing() {
        let index = MatchIndex::build(
            vec![("b1".to_string(), "Monitoring the queue".to_string())],
            vec![
                ("e-it".to_string(), "IT".to_string()),
                ("e-empty".to_string(), String::new()),
                ("e-ok".to_string(), "queue".to_string()),
            ],
        );
        // "IT" is inside "monitoring" and an empty name is inside everything; neither is
        // an entity worth matching, so neither is indexed at all.
        let hits = index.matches("Monitoring", "fig");
        assert_eq!(hits, vec![("b1", "block_text")], "{hits:?}");
        // A name that clears the bar still matches, in both directions: the fragment
        // can contain the name, or be contained by it.
        assert_eq!(index.matches("queue depth", "fig"), vec![("e-ok", "entity_name")]);
        assert_eq!(index.matches("queue", "fig"), vec![("b1", "block_text"), ("e-ok", "entity_name")]);
    }

    #[test]
    fn the_rarest_token_candidate_set_loses_no_match() {
        // "the" is in every block, "roberta" in one. Whichever token the fragment is
        // reduced to, the block containing the whole phrase must still be found.
        let blocks: Vec<(String, String)> = (0..50)
            .map(|i| (format!("b{i}"), format!("the block number {i} of the document")))
            .chain(std::iter::once(("hit".to_string(), "the roberta model".to_string())))
            .collect();
        let index = MatchIndex::build(blocks, vec![]);
        assert_eq!(index.matches("the roberta", "fig"), vec![("hit", "block_text")]);
        // A fragment whose tokens appear in no block matches nothing.
        assert!(index.matches("nonexistent phrase", "fig").is_empty());
    }

    #[test]
    fn image_kinds() {
        assert!(is_image_kind(BlockKind::Figure) && is_image_kind(BlockKind::Chart));
        assert!(!is_image_kind(BlockKind::Table) && !is_image_kind(BlockKind::Paragraph));
    }
}
