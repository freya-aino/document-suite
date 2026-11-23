//! Tables as a first-class source: rows of result tables become `result` frames.
//!
//! Heuristics (deliberately modest): the header row must contain at least one metric-like
//! column; most cells in those columns must be numeric; the first column names the method
//! (row label). Evidence is a span on the table block's plain text (cell text as quote).

use vs_core::{BlockContent, Evidence};
use vs_stitch::{LinkKind, StitchedDocument};

use crate::ground::locate_span;
use crate::model::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, Fragment, Mention, Qualifiers};

const METRIC_WORDS: &[&str] = &[
    "accuracy", "acc", "acc.", "f1", "f-1", "f1-score", "precision", "recall", "bleu", "rouge", "rouge-l", "rouge-1", "rouge-2", "meteor", "cider",
    "map", "map50", "ap", "ap50", "ap75", "auc", "auroc", "auprc", "mse", "rmse", "mae", "mape", "wer", "cer", "ter", "perplexity", "ppl", "score",
    "error", "error rate", "loss", "em", "exact match", "top-1", "top-5", "top1", "top5", "iou", "miou", "psnr", "ssim", "fid", "latency", "throughput",
    "fps", "time", "runtime", "speed", "params", "flops", "r2", "r²", "pearson", "spearman", "kappa", "dice", "ndcg", "mrr", "hits@1", "hits@10", "ece",
    "nll", "bits/dim", "bpd", "cost", "revenue", "margin", "yield", "efficiency",
];

/// A parsed table: rows of plain-text cells; `header` is true when the row used `<th>`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub cells: Vec<String>,
    pub header: bool,
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Minimal `<tr>/<td>/<th>` scan. Ignores rowspan/colspan.
pub fn parse_html_table(html: &str) -> Vec<TableRow> {
    let lower = html.to_ascii_lowercase();
    let mut rows = vec![];
    let mut pos = 0;
    while let Some(rs) = lower[pos..].find("<tr") {
        let row_start = pos + rs;
        let content_start = match lower[row_start..].find('>') {
            Some(p) => row_start + p + 1,
            None => break,
        };
        let row_end = lower[content_start..].find("</tr>").map(|p| content_start + p).unwrap_or(lower.len());
        let row_html = &html[content_start..row_end];
        let row_lower = &lower[content_start..row_end];
        let mut cells = vec![];
        let mut header = false;
        let mut cp = 0;
        loop {
            let td = row_lower[cp..].find("<td");
            let th = row_lower[cp..].find("<th");
            let (off, is_th) = match (td, th) {
                (Some(a), Some(b)) => (a.min(b), b < a),
                (Some(a), None) => (a, false),
                (None, Some(b)) => (b, true),
                (None, None) => break,
            };
            let cell_start = cp + off;
            let Some(gt) = row_lower[cell_start..].find('>') else { break };
            let cs = cell_start + gt + 1;
            let close = if is_th { "</th>" } else { "</td>" };
            let ce = row_lower[cs..].find(close).map(|p| cs + p).or_else(|| row_lower[cs..].find("<t").map(|p| cs + p)).unwrap_or(row_lower.len());
            cells.push(strip_tags(&row_html[cs..ce]));
            header |= is_th;
            cp = ce.max(cs);
            if cp >= row_lower.len() {
                break;
            }
        }
        if !cells.is_empty() {
            rows.push(TableRow { cells, header });
        }
        pos = row_end.max(content_start);
    }
    rows
}

pub fn is_metric_header(cell: &str) -> bool {
    let c = cell.trim().to_lowercase();
    if c.is_empty() {
        return false;
    }
    if c.contains('%') {
        return true;
    }
    let base = c.split(['(', '[', '↑', '↓']).next().unwrap_or("").trim();
    METRIC_WORDS.contains(&base) || base.split([' ', '/', '-']).any(|w| METRIC_WORDS.contains(&w) && w.len() > 2)
}

/// Parses `92.1`, `92.1%`, `0.87 ± 0.02`, `**91.3**`, `91.3*`, `−3.2`, `1,234`. Returns `(number text, unit)`.
pub fn parse_numeric_cell(cell: &str) -> Option<(String, Option<String>)> {
    let s = cell.trim().trim_matches(|c: char| c == '*' || c == '†' || c == '‡' || c == '_');
    let s = s.split(['±', '(']).next().unwrap_or("").trim();
    if s.is_empty() || s == "-" || s == "–" {
        return None;
    }
    let (num, unit) = if let Some(n) = s.strip_suffix('%') {
        (n.trim(), Some("%".to_string()))
    } else {
        let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',' || c == '-' || c == '−' || c == '+'));
        match split {
            Some(p) if p > 0 => (s[..p].trim(), Some(s[p..].trim().to_string()).filter(|u| !u.is_empty() && u.chars().count() <= 6)),
            Some(_) => return None,
            None => (s, None),
        }
    };
    let cleaned: String = num.replace('−', "-").replace(',', "");
    if cleaned.is_empty() || cleaned.parse::<f64>().is_err() {
        return None;
    }
    Some((cleaned, unit))
}

/// Emits `result` frames for result-like tables.
pub fn extract_tables(doc: &StitchedDocument, provenance_idx: usize) -> Fragment {
    let mut frag = Fragment::default();
    let mut ids = 0usize;
    for (i, b) in doc.blocks.iter().enumerate() {
        if !doc.is_body(i) {
            continue;
        }
        let html = match &b.block.content {
            BlockContent::Table { html } => html.as_str(),
            BlockContent::Chart { data_html: Some(html), .. } => html.as_str(),
            _ => continue,
        };
        let rows = parse_html_table(html);
        if rows.len() < 2 {
            continue;
        }
        let header_idx = rows.iter().position(|r| r.header).unwrap_or(0);
        let header = &rows[header_idx];
        let metric_cols: Vec<usize> = header.cells.iter().enumerate().filter(|(k, c)| *k > 0 && is_metric_header(c)).map(|(k, _)| k).collect();
        if metric_cols.is_empty() {
            continue;
        }
        // Kept with their row index, because a cell's citation is (row, column) and the
        // filtered list's position is not the row's position in the table.
        let body: Vec<(usize, &TableRow)> =
            rows.iter().enumerate().skip(header_idx + 1).filter(|(_, r)| !r.header && r.cells.len() > 1).collect();
        if body.is_empty() {
            continue;
        }
        let (mut numeric, mut total) = (0usize, 0usize);
        for (_, r) in &body {
            for &k in &metric_cols {
                if let Some(c) = r.cells.get(k) {
                    total += 1;
                    numeric += parse_numeric_cell(c).is_some() as usize;
                }
            }
        }
        if total == 0 || numeric * 2 < total {
            continue;
        }
        // Row labels must mostly be non-numeric.
        if body.iter().filter(|(_, r)| parse_numeric_cell(&r.cells[0]).is_some()).count() * 2 > body.len() {
            continue;
        }
        let caption = doc
            .links
            .iter()
            .find(|l| l.kind == LinkKind::CaptionOf && l.to == b.block.block_hk)
            .and_then(|l| doc.blocks.iter().find(|c| c.block.block_hk == l.from))
            .map(|c| c.block.text.trim().to_string());
        let text = &b.block.text;
        let text_len = text.chars().count() as u32;
        // Used for the *mentions* of a column header and a row label, where a span is the
        // right instrument: those are names, they occur once, and a reader highlighting
        // them wants the characters. The numbers they intersect at are cited as cells.
        let span_of = |cell: &str| locate_span(text, cell).unwrap_or((0, text_len));
        let tbl = format!("tbl{i}");

        let mut metric_entities = vec![];
        for &k in &metric_cols {
            let name = header.cells[k].trim().to_string();
            ids += 1;
            let (cs, ce) = span_of(&name);
            let mid = format!("{tbl}.m{ids}");
            frag.mentions.push(Mention { id: mid.clone(), block_hk: b.block.block_hk.clone(), char_start: cs, char_end: ce, surface: name.clone(), raw_type: "metric".into(), confidence: 0.6 });
            let eid = format!("{tbl}.e{ids}");
            frag.entities.push(DocEntity { local_id: eid.clone(), name, raw_type: "metric".into(), type_id: None, aliases: vec![], mentions: vec![mid], entity_hk: None, context: String::new() });
            metric_entities.push((k, eid));
        }
        for (row_idx, r) in &body {
            let label = r.cells[0].trim();
            if label.is_empty() {
                continue;
            }
            ids += 1;
            let (cs, ce) = span_of(label);
            let mid = format!("{tbl}.m{ids}");
            frag.mentions.push(Mention { id: mid.clone(), block_hk: b.block.block_hk.clone(), char_start: cs, char_end: ce, surface: label.to_string(), raw_type: "method".into(), confidence: 0.6 });
            let method_id = format!("{tbl}.e{ids}");
            frag.entities.push(DocEntity { local_id: method_id.clone(), name: label.to_string(), raw_type: "method".into(), type_id: None, aliases: vec![], mentions: vec![mid], entity_hk: None, context: String::new() });
            for (k, metric_id) in &metric_entities {
                let Some(cell) = r.cells.get(*k) else { continue };
                let Some((value, cell_unit)) = parse_numeric_cell(cell) else { continue };
                let unit = cell_unit.or_else(|| header.cells[*k].contains('%').then(|| "%".to_string()));
                ids += 1;
                frag.assertions.push(Assertion {
                    local_id: format!("{tbl}.a{ids}"),
                    predicate: "result".into(),
                    predicate_id: None,
                    kind: AssertionKind::Nary,
                    args: vec![
                        Arg { role: "method".into(), value: ArgValue::Entity { local_id: method_id.clone() } },
                        Arg { role: "metric".into(), value: ArgValue::Entity { local_id: metric_id.clone() } },
                        Arg { role: "value".into(), value: ArgValue::Literal { value, unit } },
                    ],
                    qualifiers: Qualifiers { condition: caption.clone(), ..Default::default() },
                    // The cell itself, not a character span into the table's flattened
                    // text. `92.1` in two rows of a results table gives two identical
                    // spans, so the citation used to point at whichever came first —
                    // which for a table of numbers is most of them.
                    evidence: vec![Evidence::Cell {
                        block_hk: b.block.block_hk.clone(),
                        row: *row_idx as u32,
                        col: *k as u32,
                    }],
                    confidence: 0.6,
                    verified: false,
                    provenance_idx,
                });
            }
        }
    }
    frag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::DocBuilder;
    use vs_core::BlockKind;

    const HTML: &str = "<table><tr><th>Model</th><th>Acc (%)</th><th>F1</th><th>Params</th></tr>\
        <tr><td><b>BERT</b></td><td>92.1</td><td>0.90 ± 0.01</td><td>110M</td></tr>\
        <tr><td>RoBERTa</td><td>94.3*</td><td>0.93</td><td>125M</td></tr></table>";

    #[test]
    fn parses_rows_and_cells() {
        let rows = parse_html_table(HTML);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].header);
        assert_eq!(rows[0].cells, vec!["Model", "Acc (%)", "F1", "Params"]);
        assert_eq!(rows[1].cells[0], "BERT");
        assert_eq!(parse_numeric_cell("0.90 ± 0.01"), Some(("0.90".into(), None)));
        assert_eq!(parse_numeric_cell("94.3*"), Some(("94.3".into(), None)));
        assert_eq!(parse_numeric_cell("110M"), Some(("110".into(), Some("M".into()))));
        assert_eq!(parse_numeric_cell("92.1%"), Some(("92.1".into(), Some("%".into()))));
        assert_eq!(parse_numeric_cell("1,234"), Some(("1234".into(), None)));
        assert_eq!(parse_numeric_cell("BERT"), None);
        assert_eq!(parse_numeric_cell("-"), None);
        assert!(is_metric_header("Acc (%)"));
        assert!(is_metric_header("F1"));
        assert!(is_metric_header("Params"));
        assert!(!is_metric_header("Model"));
        assert!(!is_metric_header("Year"));
    }

    #[test]
    fn emits_result_frames() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Heading, "4 Results");
        let text = "Model Acc (%) F1 Params\nBERT 92.1 0.90 ± 0.01 110M\nRoBERTa 94.3* 0.93 125M";
        let t = b.block_with(BlockKind::Table, text, BlockContent::Table { html: HTML.into() });
        let c = b.block(BlockKind::Caption, "Table 2: GLUE results.");
        b.link(LinkKind::CaptionOf, c, t);
        let frag = extract_tables(&b.doc, 3);
        // 3 metric entities + 2 methods
        assert_eq!(frag.entities.len(), 5);
        assert_eq!(frag.assertions.len(), 6);
        let a = &frag.assertions[0];
        assert_eq!(a.predicate, "result");
        assert_eq!(a.kind, AssertionKind::Nary);
        assert_eq!(a.provenance_idx, 3);
        assert_eq!(a.qualifiers.condition.as_deref(), Some("Table 2: GLUE results."));
        assert_eq!(a.args[2].value, ArgValue::Literal { value: "92.1".into(), unit: Some("%".into()) });
        // The cell, by row and column: row 1 (the header is row 0), column 1 (`Acc (%)`).
        // A character span into the flattened table text cannot do this — `0.93` and a
        // repeated `92.1` give identical spans — and the row and column are what
        // `vault.hub_cell` and the graph's `Cell` nodes are keyed by, so the citation
        // resolves to something a reader can fetch.
        assert_eq!(a.evidence, vec![Evidence::Cell { block_hk: b.doc.blocks[t].block.block_hk.clone(), row: 1, col: 1 }]);
        let method = frag.entities.iter().find(|e| e.name == "BERT").unwrap();
        assert_eq!(method.raw_type, "method");
        // The method's own *mention* is still a span, because a row label is a name and a
        // reader highlighting it wants the characters.
        let mention = &frag.mentions[frag.mentions.len() - 1];
        let quote: String =
            text.chars().skip(mention.char_start as usize).take((mention.char_end - mention.char_start) as usize).collect();
        assert_eq!(quote, "RoBERTa");

        // Every cell citation points at a cell that exists in the table.
        let rows = parse_html_table(HTML);
        for a in &frag.assertions {
            let Evidence::Cell { row, col, .. } = &a.evidence[0] else { panic!("table assertions cite cells") };
            let cell = rows.get(*row as usize).and_then(|r| r.cells.get(*col as usize)).expect("a cell that exists");
            assert!(!cell.trim().is_empty(), "cited an empty cell at ({row}, {col})");
        }
    }

    #[test]
    fn skips_non_result_tables() {
        let mut b = DocBuilder::new();
        let html = "<table><tr><th>Name</th><th>Country</th></tr><tr><td>A</td><td>DE</td></tr></table>";
        b.block_with(BlockKind::Table, "Name Country\nA DE", BlockContent::Table { html: html.into() });
        let html2 = "<table><tr><th>Model</th><th>Acc</th></tr><tr><td>A</td><td>n/a</td></tr><tr><td>B</td><td>tbd</td></tr></table>";
        b.block_with(BlockKind::Table, "x", BlockContent::Table { html: html2.into() });
        assert!(extract_tables(&b.doc, 0).is_empty());
    }
}
