//! Page-level metrics (SPEC.md "Scoring"): edit-distance text similarity, TEDS for
//! tables, LaTeX similarity, reading order, layout F1 at IoU 0.5.

use vs_core::{BBox, BlockKind};

// ---------------------------------------------------------------------------
// Edit distance
// ---------------------------------------------------------------------------

/// Levenshtein distance over any comparable sequence (two-row DP).
pub fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(x != y);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// `1 − levenshtein / max(len)` over chars; two empty strings are identical (1.0).
pub fn text_similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len().max(b.len());
    if n == 0 {
        return 1.0;
    }
    1.0 - levenshtein(&a, &b) as f32 / n as f32
}

/// Collapses whitespace runs to one space and trims.
pub fn normalise_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whitespace-insensitive text similarity (used for reading-order text and LaTeX).
pub fn normalised_text_similarity(a: &str, b: &str) -> f32 {
    text_similarity(&normalise_ws(a), &normalise_ws(b))
}

/// LaTeX similarity: whitespace removed entirely (`a^{2}` vs `a^{ 2 }` are equal), then
/// normalised edit distance. An approximation of CDM, which needs a renderer.
pub fn latex_similarity(a: &str, b: &str) -> f32 {
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    text_similarity(&strip(a), &strip(b))
}

// ---------------------------------------------------------------------------
// TEDS
// ---------------------------------------------------------------------------

/// Node of a simplified HTML table tree. Only structural tags are kept
/// (`table`, `thead`, `tbody`, `tfoot`, `tr`, `td`, `th`); every other tag is dropped
/// and its text merged into the enclosing cell. `th` is treated as `td`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableNode {
    pub tag: String,
    pub colspan: u32,
    pub rowspan: u32,
    /// Cell text (cells only).
    pub text: String,
    pub children: Vec<TableNode>,
}

impl TableNode {
    fn new(tag: &str) -> Self {
        Self { tag: tag.to_string(), colspan: 1, rowspan: 1, text: String::new(), children: vec![] }
    }

    pub fn size(&self) -> usize {
        1 + self.children.iter().map(TableNode::size).sum::<usize>()
    }
}

fn attr_u32(attrs: &str, name: &str) -> Option<u32> {
    let lower = attrs.to_ascii_lowercase();
    let idx = lower.find(name)?;
    let rest = &attrs[idx + name.len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let val: String = rest.trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_digit()).collect();
    val.parse().ok()
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// Parses HTML into a table tree rooted at `table`. Tolerant: unknown tags are ignored
/// (their text is merged into the enclosing cell), missing `<table>`/`<tr>` wrappers are
/// synthesised, unclosed cells and rows are closed by the next structural tag.
/// Returns `None` if no cell is found.
pub fn parse_table(html: &str) -> Option<TableNode> {
    let mut stack: Vec<TableNode> = vec![TableNode::new("table")];
    let mut cell_text = String::new();
    let mut found_cell = false;

    fn is_open(stack: &[TableNode], tag: &str) -> bool {
        stack[1..].iter().any(|n| n.tag == tag)
    }
    fn in_cell(stack: &[TableNode]) -> bool {
        stack.last().is_some_and(|n| n.tag == "td")
    }
    /// Pops nodes into their parents until `tag` is closed (no-op if it is not open).
    fn close(stack: &mut Vec<TableNode>, tag: &str, cell_text: &mut String) {
        if !is_open(stack, tag) {
            return;
        }
        loop {
            let mut node = stack.pop().expect("root is never popped");
            if node.tag == "td" {
                node.text = normalise_ws(&decode_entities(cell_text));
                cell_text.clear();
            }
            let done = node.tag == tag;
            stack.last_mut().expect("root").children.push(node);
            if done {
                break;
            }
        }
    }

    let mut rest = html;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('<') {
            let Some(end) = stripped.find('>') else { break };
            let inner = stripped[..end].trim();
            rest = &stripped[end + 1..];
            if inner.starts_with('!') || inner.starts_with('?') {
                continue;
            }
            let closing = inner.starts_with('/');
            let body = inner.trim_start_matches('/').trim_end_matches('/');
            let (name, attrs) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
            let name = match name.to_ascii_lowercase().as_str() {
                "th" => "td".to_string(),
                n => n.to_string(),
            };
            match (name.as_str(), closing) {
                ("td", false) => {
                    if in_cell(&stack) {
                        close(&mut stack, "td", &mut cell_text);
                    }
                    if !is_open(&stack, "tr") {
                        stack.push(TableNode::new("tr"));
                    }
                    let mut cell = TableNode::new("td");
                    cell.colspan = attr_u32(attrs, "colspan").unwrap_or(1).max(1);
                    cell.rowspan = attr_u32(attrs, "rowspan").unwrap_or(1).max(1);
                    stack.push(cell);
                    found_cell = true;
                }
                ("tr", false) => {
                    close(&mut stack, "td", &mut cell_text);
                    close(&mut stack, "tr", &mut cell_text);
                    stack.push(TableNode::new("tr"));
                }
                ("thead" | "tbody" | "tfoot", false) => {
                    close(&mut stack, "td", &mut cell_text);
                    close(&mut stack, "tr", &mut cell_text);
                    for section in ["thead", "tbody", "tfoot"] {
                        close(&mut stack, section, &mut cell_text);
                    }
                    stack.push(TableNode::new(&name));
                }
                ("td" | "tr" | "thead" | "tbody" | "tfoot", true) => close(&mut stack, &name, &mut cell_text),
                ("br" | "p" | "div", _) if in_cell(&stack) => cell_text.push(' '),
                _ => {} // table, b, i, span, ... carry no structure
            }
        } else {
            let next = rest.find('<').unwrap_or(rest.len());
            if in_cell(&stack) {
                cell_text.push_str(&rest[..next]);
            }
            rest = &rest[next..];
        }
    }
    while stack.len() > 1 {
        let tag = stack.last().expect("non-empty").tag.clone();
        close(&mut stack, &tag, &mut cell_text);
    }
    if !found_cell {
        return None;
    }
    stack.pop()
}

/// Flattened tree in post-order for Zhang–Shasha.
struct Flat<'a> {
    nodes: Vec<&'a TableNode>,
    /// leftmost leaf descendant index per node
    lmd: Vec<usize>,
    keyroots: Vec<usize>,
}

fn flatten(root: &TableNode) -> Flat<'_> {
    fn walk<'a>(n: &'a TableNode, nodes: &mut Vec<&'a TableNode>, lmd: &mut Vec<usize>) -> usize {
        let mut first_leaf = None;
        for c in &n.children {
            let l = walk(c, nodes, lmd);
            first_leaf.get_or_insert(l);
        }
        nodes.push(n);
        let idx = nodes.len() - 1;
        let leftmost = first_leaf.unwrap_or(idx);
        lmd.push(leftmost);
        leftmost
    }
    let mut nodes = vec![];
    let mut lmd = vec![];
    walk(root, &mut nodes, &mut lmd);
    // keyroots: highest node for each distinct lmd
    let mut keyroots = vec![];
    for i in 0..nodes.len() {
        if !(i + 1..nodes.len()).any(|j| lmd[j] == lmd[i]) {
            keyroots.push(i);
        }
    }
    Flat { nodes, lmd, keyroots }
}

/// Relabel cost: 1 if tag or spans differ; for cells, the normalised edit distance of
/// their text; 0 otherwise.
fn relabel_cost(a: &TableNode, b: &TableNode) -> f32 {
    if a.tag != b.tag || a.colspan != b.colspan || a.rowspan != b.rowspan {
        return 1.0;
    }
    if a.tag == "td" {
        1.0 - text_similarity(&a.text, &b.text)
    } else {
        0.0
    }
}

/// Zhang–Shasha ordered tree edit distance with unit insert/delete and `relabel_cost`.
pub fn tree_edit_distance(a: &TableNode, b: &TableNode) -> f32 {
    let fa = flatten(a);
    let fb = flatten(b);
    let (n, m) = (fa.nodes.len(), fb.nodes.len());
    let mut td = vec![vec![0f32; m]; n];
    for &i in &fa.keyroots {
        for &j in &fb.keyroots {
            // forest distance over subtrees rooted at i, j
            let (li, lj) = (fa.lmd[i], fb.lmd[j]);
            let rows = i - li + 2;
            let cols = j - lj + 2;
            let mut fd = vec![vec![0f32; cols]; rows];
            for x in 1..rows {
                fd[x][0] = fd[x - 1][0] + 1.0;
            }
            for y in 1..cols {
                fd[0][y] = fd[0][y - 1] + 1.0;
            }
            for x in 1..rows {
                for y in 1..cols {
                    let (ni, nj) = (li + x - 1, lj + y - 1);
                    if fa.lmd[ni] == li && fb.lmd[nj] == lj {
                        let c = relabel_cost(fa.nodes[ni], fb.nodes[nj]);
                        fd[x][y] = (fd[x - 1][y] + 1.0).min(fd[x][y - 1] + 1.0).min(fd[x - 1][y - 1] + c);
                        td[ni][nj] = fd[x][y];
                    } else {
                        let px = fa.lmd[ni] - li; // forest prefix sizes
                        let py = fb.lmd[nj] - lj;
                        fd[x][y] = (fd[x - 1][y] + 1.0).min(fd[x][y - 1] + 1.0).min(fd[px][py] + td[ni][nj]);
                    }
                }
            }
        }
    }
    td[n - 1][m - 1]
}

/// Tree-Edit-Distance-based Similarity: `1 − TED / max(|T_pred|, |T_gt|)`.
/// Unparseable prediction → 0; both unparseable → 0 (nothing to compare).
pub fn teds(pred_html: &str, gt_html: &str) -> f32 {
    let (Some(p), Some(g)) = (parse_table(pred_html), parse_table(gt_html)) else { return 0.0 };
    let n = p.size().max(g.size()) as f32;
    (1.0 - tree_edit_distance(&p, &g) / n).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Boxes
// ---------------------------------------------------------------------------

/// A block as the metrics see it (prediction or ground truth).
#[derive(Debug, Clone)]
pub struct EvalBlock {
    pub kind: BlockKind,
    pub bbox: BBox,
    pub order: u32,
    pub text: String,
    pub html: Option<String>,
    pub latex: Option<String>,
}

/// Greedy one-to-one matching by IoU (highest first). Returns `(pred_idx, gt_idx, iou)`
/// for pairs with IoU ≥ `min_iou`.
pub fn match_by_iou(pred: &[EvalBlock], gt: &[EvalBlock], min_iou: f32, same_kind: bool) -> Vec<(usize, usize, f32)> {
    let mut pairs: Vec<(usize, usize, f32)> = vec![];
    for (i, p) in pred.iter().enumerate() {
        for (j, g) in gt.iter().enumerate() {
            if same_kind && !kind_family_eq(p.kind, g.kind) {
                continue;
            }
            let iou = p.bbox.intersection_over_union(&g.bbox);
            if iou >= min_iou {
                pairs.push((i, j, iou));
            }
        }
    }
    pairs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    let mut used_p = vec![false; pred.len()];
    let mut used_g = vec![false; gt.len()];
    let mut out = vec![];
    for (i, j, iou) in pairs {
        if !used_p[i] && !used_g[j] {
            used_p[i] = true;
            used_g[j] = true;
            out.push((i, j, iou));
        }
    }
    out
}

/// Kinds that count as the same for layout F1 (benchmarks label these inconsistently).
pub fn kind_family_eq(a: BlockKind, b: BlockKind) -> bool {
    use BlockKind::*;
    let fam = |k: BlockKind| match k {
        Title | Heading => 0,
        Paragraph | List | Footnote | Reference | Handwriting | Other | Code => 1,
        Table => 2,
        Figure | Chart => 3,
        Formula => 4,
        Caption => 5,
        Header | Footer | PageNumber => 6,
        Stamp => 7,
    };
    fam(a) == fam(b)
}

/// Micro F1 over all blocks: a prediction is a true positive if it matches a ground-truth
/// block of the same kind family with IoU ≥ 0.5 (one-to-one).
pub fn layout_f1(pred: &[EvalBlock], gt: &[EvalBlock]) -> f32 {
    if pred.is_empty() && gt.is_empty() {
        return 1.0;
    }
    let tp = match_by_iou(pred, gt, 0.5, true).len() as f32;
    let (p, g) = (pred.len() as f32, gt.len() as f32);
    if tp == 0.0 {
        return 0.0;
    }
    let precision = tp / p;
    let recall = tp / g;
    2.0 * precision * recall / (precision + recall)
}

/// Reading order: match predicted blocks to ground truth by IoU (≥ 0.5, any kind), list
/// the matched ground-truth indices in predicted order and compare with the ground-truth
/// order by edit distance, normalised by the number of ground-truth blocks. Unmatched
/// ground-truth blocks therefore count as errors.
pub fn reading_order_score(pred: &[EvalBlock], gt: &[EvalBlock]) -> f32 {
    if gt.is_empty() {
        return if pred.is_empty() { 1.0 } else { 0.0 };
    }
    let mut gt_sorted: Vec<usize> = (0..gt.len()).collect();
    gt_sorted.sort_by_key(|&j| gt[j].order);
    let rank: Vec<usize> = {
        let mut r = vec![0; gt.len()];
        for (pos, &j) in gt_sorted.iter().enumerate() {
            r[j] = pos;
        }
        r
    };
    let mut matched = match_by_iou(pred, gt, 0.5, false);
    matched.sort_by_key(|(i, _, _)| pred[*i].order);
    let seq: Vec<usize> = matched.iter().map(|(_, j, _)| rank[*j]).collect();
    let reference: Vec<usize> = (0..gt.len()).collect();
    let n = gt.len().max(seq.len());
    1.0 - levenshtein(&seq, &reference) as f32 / n as f32
}

/// Text of the page in reading order: text-like blocks only (tables, formulas and
/// figures are scored by their own metrics).
pub fn reading_order_text(blocks: &[EvalBlock]) -> String {
    let mut bs: Vec<&EvalBlock> = blocks.iter().filter(|b| is_text_kind(b.kind)).collect();
    bs.sort_by_key(|b| b.order);
    normalise_ws(&bs.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n"))
}

pub fn is_text_kind(k: BlockKind) -> bool {
    !matches!(k, BlockKind::Table | BlockKind::Formula | BlockKind::Figure | BlockKind::Chart)
}

/// Mean over ground-truth blocks of `kind` of `score(best IoU-overlapping prediction)`;
/// a ground-truth block without an overlapping prediction scores 0. `None` if the ground
/// truth has no such block.
pub fn per_block_mean(pred: &[EvalBlock], gt: &[EvalBlock], kind: BlockKind, score: impl Fn(&EvalBlock, &EvalBlock) -> f32) -> Option<f32> {
    let gts: Vec<&EvalBlock> = gt.iter().filter(|b| b.kind == kind).collect();
    if gts.is_empty() {
        return None;
    }
    let preds: Vec<&EvalBlock> = pred.iter().filter(|b| b.kind == kind).collect();
    let mut total = 0.0;
    for g in &gts {
        let best = preds
            .iter()
            .filter(|p| p.bbox.intersection_over_union(&g.bbox) > 0.0)
            .max_by(|a, b| a.bbox.intersection_over_union(&g.bbox).partial_cmp(&b.bbox.intersection_over_union(&g.bbox)).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(p) = best {
            total += score(p, g);
        }
    }
    Some(total / gts.len() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(kind: BlockKind, b: (f32, f32, f32, f32), order: u32, text: &str) -> EvalBlock {
        EvalBlock { kind, bbox: BBox::new(b.0, b.1, b.2, b.3).unwrap(), order, text: text.into(), html: None, latex: None }
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein(b"kitten", b"sitting"), 3);
        assert_eq!(levenshtein::<u8>(b"", b"abc"), 3);
        assert_eq!(levenshtein(&[1, 2, 3], &[1, 2, 3]), 0);
        assert!((text_similarity("abcd", "abce") - 0.75).abs() < 1e-6);
        assert_eq!(text_similarity("", ""), 1.0);
        assert_eq!(normalised_text_similarity("a  b\nc", "a b c"), 1.0);
        assert_eq!(latex_similarity("a^{2} + b", "a^{ 2 }+b"), 1.0);
    }

    #[test]
    fn table_parse_structure() {
        let t = parse_table(r#"<table><thead><tr><th colspan="2">H</th></tr></thead><tbody><tr><td>1</td><td>2 &amp; 3</td></tr></tbody></table>"#).unwrap();
        assert_eq!(t.tag, "table");
        assert_eq!(t.children.len(), 2); // thead, tbody
        let head_cell = &t.children[0].children[0].children[0];
        assert_eq!((head_cell.tag.as_str(), head_cell.colspan, head_cell.text.as_str()), ("td", 2, "H"));
        let body_row = &t.children[1].children[0];
        assert_eq!(body_row.children.len(), 2);
        assert_eq!(body_row.children[1].text, "2 & 3");
        assert_eq!(t.size(), 8); // table, thead, tr, td, tbody, tr, td, td
        assert!(parse_table("<p>no table</p>").is_none());
    }

    #[test]
    fn table_parse_tolerates_missing_closers() {
        let t = parse_table("<table><tr><td>a<td>b<tr><td>c</table>").unwrap();
        assert_eq!(t.children.len(), 2);
        assert_eq!(t.children[0].children.len(), 2);
        assert_eq!(t.children[1].children[0].text, "c");
    }

    #[test]
    fn teds_identity_and_degradation() {
        let a = "<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>";
        assert!((teds(a, a) - 1.0).abs() < 1e-6);
        // one cell text partly wrong → relabel cost = text distance (0.5) → 1 − 0.5/7
        let b = "<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4x</td></tr></table>";
        let s = teds(b, a);
        assert!((s - (1.0 - 0.5 / 7.0)).abs() < 1e-5, "{s}");
        // one cell missing → deletion
        let c = "<table><tr><td>1</td><td>2</td></tr><tr><td>3</td></tr></table>";
        let s2 = teds(c, a);
        assert!((s2 - (1.0 - 1.0 / 7.0)).abs() < 1e-5, "{s2}");
        assert!(s2 < s);
        // a span mismatch costs a relabel
        let d = "<table><tr><td colspan=\"2\">1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>";
        assert!(teds(d, a) < 1.0);
        // totally different → low
        let e = "<table><tr><td>x</td></tr></table>";
        assert!(teds(e, a) < 0.5);
        assert_eq!(teds("garbage", a), 0.0);
    }

    #[test]
    fn tree_edit_distance_small() {
        let a = parse_table("<table><tr><td>a</td></tr></table>").unwrap(); // table, tr, td = 3 nodes
        let b = parse_table("<table><tr><td>a</td><td>b</td></tr></table>").unwrap();
        assert!((tree_edit_distance(&a, &b) - 1.0).abs() < 1e-6);
        assert_eq!(tree_edit_distance(&a, &a), 0.0);
    }

    #[test]
    fn iou_f1() {
        let gt = vec![
            blk(BlockKind::Paragraph, (0.0, 0.0, 0.5, 0.2), 0, ""),
            blk(BlockKind::Table, (0.0, 0.3, 0.5, 0.6), 1, ""),
            blk(BlockKind::Figure, (0.5, 0.3, 1.0, 0.6), 2, ""),
        ];
        let pred = vec![
            blk(BlockKind::Paragraph, (0.0, 0.0, 0.5, 0.21), 0, ""), // match
            blk(BlockKind::Paragraph, (0.0, 0.3, 0.5, 0.6), 1, ""),  // right box, wrong kind → FP + table FN
            blk(BlockKind::Chart, (0.5, 0.3, 1.0, 0.6), 2, ""),      // figure/chart family → match
        ];
        // tp=2, p=3, g=3 → F1 = 2/3
        assert!((layout_f1(&pred, &gt) - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(layout_f1(&[], &[]), 1.0);
        assert_eq!(layout_f1(&pred, &[]), 0.0);
        let m = match_by_iou(&pred, &gt, 0.5, false);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn reading_order() {
        let gt = vec![
            blk(BlockKind::Paragraph, (0.0, 0.0, 1.0, 0.2), 0, "a"),
            blk(BlockKind::Paragraph, (0.0, 0.3, 1.0, 0.5), 1, "b"),
            blk(BlockKind::Paragraph, (0.0, 0.6, 1.0, 0.8), 2, "c"),
        ];
        let good = gt.clone();
        assert_eq!(reading_order_score(&good, &gt), 1.0);
        let mut swapped = gt.clone();
        swapped[0].order = 1;
        swapped[1].order = 0;
        let s = reading_order_score(&swapped, &gt);
        assert!((s - (1.0 - 2.0 / 3.0)).abs() < 1e-6, "{s}");
        // missing block → one deletion
        let partial = vec![gt[0].clone(), gt[2].clone()];
        assert!((reading_order_score(&partial, &gt) - (1.0 - 1.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn reading_order_text_skips_tables() {
        let bs = vec![
            blk(BlockKind::Paragraph, (0.0, 0.5, 1.0, 0.6), 1, "second"),
            blk(BlockKind::Table, (0.0, 0.3, 1.0, 0.4), 0, "<table>"),
            blk(BlockKind::Heading, (0.0, 0.0, 1.0, 0.1), 0, "first"),
        ];
        assert_eq!(reading_order_text(&bs), "first second");
    }

    #[test]
    fn per_block_mean_scores_unmatched_as_zero() {
        let gt = vec![
            blk(BlockKind::Formula, (0.0, 0.0, 0.5, 0.2), 0, "x"),
            blk(BlockKind::Formula, (0.0, 0.5, 0.5, 0.7), 1, "y"),
        ];
        let pred = vec![blk(BlockKind::Formula, (0.0, 0.0, 0.5, 0.2), 0, "x")];
        let s = per_block_mean(&pred, &gt, BlockKind::Formula, |p, g| latex_similarity(&p.text, &g.text)).unwrap();
        assert!((s - 0.5).abs() < 1e-6);
        assert!(per_block_mean(&pred, &gt, BlockKind::Table, |_, _| 1.0).is_none());
    }
}
