//! Cross references (`LinkKind::RefersTo`), SPEC.md §7.
//!
//! Mentions such as "Table 3", "Fig. 2a", "Eq. (4)", "Section 3.1", "Abb. 2" or "图 2" in
//! body text are resolved to the block they name. Label words come from a per-language
//! table keyed by the page profile's primary language subtag; every language also gets the
//! English labels, and unknown languages get English only. Number syntax (`3`, `2a`,
//! `3.1`, `(4)`) is language independent.

use std::collections::{HashMap, HashSet};

use vs_core::{BlockContent, BlockKind, HashKey};

use crate::{DocBlock, Link, LinkKind, StitchedDocument};

/// What a reference label points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefTarget {
    Figure,
    Table,
    Equation,
    Section,
}

/// One label word (without trailing period) and what it refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefLabel {
    pub label: &'static str,
    pub target: RefTarget,
}

const fn l(label: &'static str, target: RefTarget) -> RefLabel {
    RefLabel { label, target }
}

const EN: &[RefLabel] = &[
    l("figures", RefTarget::Figure),
    l("figure", RefTarget::Figure),
    l("figs", RefTarget::Figure),
    l("fig", RefTarget::Figure),
    l("tables", RefTarget::Table),
    l("table", RefTarget::Table),
    l("tab", RefTarget::Table),
    l("tbl", RefTarget::Table),
    l("equations", RefTarget::Equation),
    l("equation", RefTarget::Equation),
    l("eqs", RefTarget::Equation),
    l("eqn", RefTarget::Equation),
    l("eq", RefTarget::Equation),
    l("sections", RefTarget::Section),
    l("section", RefTarget::Section),
    l("sect", RefTarget::Section),
    l("sec", RefTarget::Section),
    l("chapter", RefTarget::Section),
    l("§", RefTarget::Section),
];

const DE: &[RefLabel] = &[
    l("abbildungen", RefTarget::Figure),
    l("abbildung", RefTarget::Figure),
    l("abb", RefTarget::Figure),
    l("bild", RefTarget::Figure),
    l("tabellen", RefTarget::Table),
    l("tabelle", RefTarget::Table),
    l("tab", RefTarget::Table),
    l("gleichungen", RefTarget::Equation),
    l("gleichung", RefTarget::Equation),
    l("gl", RefTarget::Equation),
    l("abschnitt", RefTarget::Section),
    l("abschn", RefTarget::Section),
    l("kapitel", RefTarget::Section),
    l("kap", RefTarget::Section),
    l("§", RefTarget::Section),
];

const FR: &[RefLabel] = &[
    l("figure", RefTarget::Figure),
    l("fig", RefTarget::Figure),
    l("tableau", RefTarget::Table),
    l("tab", RefTarget::Table),
    l("équation", RefTarget::Equation),
    l("equation", RefTarget::Equation),
    l("éq", RefTarget::Equation),
    l("eq", RefTarget::Equation),
    l("section", RefTarget::Section),
    l("sect", RefTarget::Section),
    l("chapitre", RefTarget::Section),
    l("chap", RefTarget::Section),
    l("§", RefTarget::Section),
];

const ES: &[RefLabel] = &[
    l("figura", RefTarget::Figure),
    l("fig", RefTarget::Figure),
    l("tabla", RefTarget::Table),
    l("cuadro", RefTarget::Table),
    l("ecuación", RefTarget::Equation),
    l("ecuacion", RefTarget::Equation),
    l("ec", RefTarget::Equation),
    l("sección", RefTarget::Section),
    l("seccion", RefTarget::Section),
    l("capítulo", RefTarget::Section),
    l("cap", RefTarget::Section),
    l("§", RefTarget::Section),
];

const IT: &[RefLabel] = &[
    l("figura", RefTarget::Figure),
    l("fig", RefTarget::Figure),
    l("tabella", RefTarget::Table),
    l("tab", RefTarget::Table),
    l("equazione", RefTarget::Equation),
    l("eq", RefTarget::Equation),
    l("sezione", RefTarget::Section),
    l("sez", RefTarget::Section),
    l("capitolo", RefTarget::Section),
    l("cap", RefTarget::Section),
    l("§", RefTarget::Section),
];

const PT: &[RefLabel] = &[
    l("figura", RefTarget::Figure),
    l("fig", RefTarget::Figure),
    l("tabela", RefTarget::Table),
    l("tab", RefTarget::Table),
    l("equação", RefTarget::Equation),
    l("equacao", RefTarget::Equation),
    l("eq", RefTarget::Equation),
    l("seção", RefTarget::Section),
    l("secao", RefTarget::Section),
    l("capítulo", RefTarget::Section),
    l("cap", RefTarget::Section),
    l("§", RefTarget::Section),
];

const NL: &[RefLabel] = &[
    l("figuur", RefTarget::Figure),
    l("fig", RefTarget::Figure),
    l("afbeelding", RefTarget::Figure),
    l("afb", RefTarget::Figure),
    l("tabel", RefTarget::Table),
    l("tab", RefTarget::Table),
    l("vergelijking", RefTarget::Equation),
    l("vgl", RefTarget::Equation),
    l("paragraaf", RefTarget::Section),
    l("sectie", RefTarget::Section),
    l("hoofdstuk", RefTarget::Section),
    l("§", RefTarget::Section),
];

const ZH: &[RefLabel] = &[
    l("图", RefTarget::Figure),
    l("圖", RefTarget::Figure),
    l("表", RefTarget::Table),
    l("公式", RefTarget::Equation),
    l("式", RefTarget::Equation),
    l("第", RefTarget::Section), // "第3节" / "第3章": number follows 第, see `strip_section_suffix`
    l("节", RefTarget::Section),
    l("章", RefTarget::Section),
];

const JA: &[RefLabel] = &[
    l("図", RefTarget::Figure),
    l("表", RefTarget::Table),
    l("式", RefTarget::Equation),
    l("第", RefTarget::Section),
    l("節", RefTarget::Section),
    l("章", RefTarget::Section),
];

/// Labels for a primary language subtag (lowercase), always including English.
pub fn labels_for(lang: &str) -> Vec<RefLabel> {
    let own: &[RefLabel] = match lang {
        "en" => &[],
        "de" => DE,
        "fr" => FR,
        "es" => ES,
        "it" => IT,
        "pt" => PT,
        "nl" => NL,
        "zh" => ZH,
        "ja" => JA,
        _ => &[],
    };
    let mut out: Vec<RefLabel> = own.to_vec();
    for e in EN {
        if !out.iter().any(|x| x.label == e.label && x.target == e.target) {
            out.push(*e);
        }
    }
    // Longest labels first so "figure" wins over "fig" and "tables" over "table".
    out.sort_by_key(|x| std::cmp::Reverse(x.label.chars().count()));
    out
}

/// A reference found in text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub target: RefTarget,
    /// The number as written, without parentheses: `3`, `2a`, `3.1`.
    pub number: String,
    /// Character offset of the label start in the scanned text.
    pub char_start: usize,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Reads a reference number at `pos` (chars). Accepts `3`, `2a`, `3.1.2`, `(4)`, `IV` is
/// not supported. Returns the number and the position after it.
fn read_number(chars: &[char], mut pos: usize) -> Option<(String, usize)> {
    while pos < chars.len() && (chars[pos].is_whitespace() || matches!(chars[pos], '~' | '\u{a0}')) {
        pos += 1;
    }
    let mut parens = false;
    if pos < chars.len() && chars[pos] == '(' {
        parens = true;
        pos += 1;
    }
    let start = pos;
    while pos < chars.len() && chars[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos == start {
        return None;
    }
    // dotted sub-numbers: 3.1.2 (a trailing '.' is sentence punctuation, not part of the number)
    while pos + 1 < chars.len() && chars[pos] == '.' && chars[pos + 1].is_ascii_digit() {
        pos += 1;
        while pos < chars.len() && chars[pos].is_ascii_digit() {
            pos += 1;
        }
    }
    // sub-figure letter: 2a, 2b (one letter, not followed by another word char)
    if pos < chars.len()
        && chars[pos].is_ascii_lowercase()
        && chars.get(pos + 1).map_or(true, |c| !is_word_char(*c))
    {
        pos += 1;
    }
    let number: String = chars[start..pos].iter().collect();
    if parens {
        if pos < chars.len() && chars[pos] == ')' {
            pos += 1;
        } else {
            return None;
        }
    }
    Some((number, pos))
}

/// Finds reference mentions in `text` using `labels`. Labels match case-insensitively at
/// word boundaries (for alphabetic labels) or anywhere (CJK), optionally followed by a
/// period, then a number. Overlapping matches keep the first (longest label).
pub fn find_mentions(text: &str, labels: &[RefLabel]) -> Vec<Mention> {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    // `to_lowercase` may change char count for exotic scripts; fall back to per-char lowering.
    let lower: Vec<char> = if lower.len() == chars.len() {
        lower
    } else {
        chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect()
    };
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        let mut matched = None;
        for lab in labels {
            let lc: Vec<char> = lab.label.chars().collect();
            if pos + lc.len() > lower.len() || lower[pos..pos + lc.len()] != lc[..] {
                continue;
            }
            let alphabetic = lc[0].is_ascii_alphabetic();
            if alphabetic {
                if pos > 0 && is_word_char(chars[pos - 1]) {
                    continue;
                }
                let after = pos + lc.len();
                if after < chars.len() && is_word_char(chars[after]) {
                    continue;
                }
            }
            let mut p = pos + lc.len();
            if p < chars.len() && chars[p] == '.' {
                p += 1;
            }
            if let Some((number, end)) = read_number(&chars, p) {
                let end = if lab.label == "第" { skip_cjk_section_suffix(&chars, end) } else { end };
                matched = Some((Mention { target: lab.target, number, char_start: pos }, end));
                break;
            }
        }
        match matched {
            Some((m, end)) => {
                out.push(m);
                pos = end;
            }
            None => pos += 1,
        }
    }
    out
}

fn skip_cjk_section_suffix(chars: &[char], pos: usize) -> usize {
    if pos < chars.len() && matches!(chars[pos], '节' | '節' | '章' | '部') {
        pos + 1
    } else {
        pos
    }
}

/// If `text` starts with a label of `target` followed by a number, returns that number.
pub fn leading_label_number(text: &str, labels: &[RefLabel], target: RefTarget) -> Option<String> {
    let trimmed = text.trim_start();
    let m = find_mentions(trimmed, labels).into_iter().next()?;
    (m.char_start == 0 && m.target == target).then_some(m.number)
}

fn numbers_match(mention: &str, label: &str) -> bool {
    if mention == label {
        return true;
    }
    // "Figure 2a" refers to the caption "Figure 2"
    let base = mention.trim_end_matches(|c: char| c.is_ascii_lowercase());
    !base.is_empty() && base == label
}

/// Heading numbering such as "3.1" from "3.1 Scope" or "3.1. Scope".
fn heading_number(title: &str) -> Option<String> {
    let token = title.split_whitespace().next()?.trim_end_matches('.');
    let ok = !token.is_empty() && token.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    ok.then(|| token.to_string())
}

/// Equation number from a formula: `\tag{4}`, `\tag*{4}` or a trailing `(4)`.
fn formula_number(block: &vs_core::Block) -> Option<String> {
    let latex = match &block.content {
        BlockContent::Formula { latex } => latex.as_str(),
        _ => block.text.as_str(),
    };
    for src in [latex, block.text.as_str()] {
        if let Some(i) = src.find("\\tag") {
            let rest = &src[i + 4..];
            let rest = rest.strip_prefix('*').unwrap_or(rest);
            if let Some(inner) = rest.strip_prefix('{').and_then(|r| r.split_once('}')).map(|(a, _)| a) {
                return Some(inner.trim().to_string());
            }
        }
        let t = src.trim_end();
        if let Some(t) = t.strip_suffix(')') {
            if let Some(i) = t.rfind('(') {
                let n = t[i + 1..].trim();
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') && n.starts_with(|c: char| c.is_ascii_digit()) {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

fn is_body_text(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Paragraph | BlockKind::List | BlockKind::Footnote)
}

/// Picks the candidate nearest in reading order. Returns `(block index, unique)`.
fn nearest(candidates: &[usize], from: usize) -> Option<(usize, bool)> {
    let best = *candidates.iter().min_by_key(|&&i| (i as i64 - from as i64).abs())?;
    Some((best, candidates.len() == 1))
}

/// Resolves mentions in body text blocks and appends `RefersTo` links.
/// `languages` maps `page_no` to the page profile's primary language subtag.
pub fn link_cross_refs(doc: &mut StitchedDocument, languages: &HashMap<u32, String>) {
    let caption_target: HashMap<&HashKey, &HashKey> = doc
        .links
        .iter()
        .filter(|l| l.kind == LinkKind::CaptionOf)
        .map(|l| (&l.from, &l.to))
        .collect();
    let blocks = &doc.blocks;
    let body: Vec<usize> = (0..blocks.len()).filter(|&i| doc.is_body(i)).collect();
    let mut label_cache: HashMap<String, Vec<RefLabel>> = HashMap::new();
    let mut links = Vec::new();
    let mut seen: HashSet<(HashKey, HashKey)> = HashSet::new();

    for &i in &body {
        let src = &blocks[i];
        if !is_body_text(src.block.kind) || src.block.text.is_empty() {
            continue;
        }
        let lang = languages.get(&src.page_no).cloned().unwrap_or_default();
        let labels = label_cache.entry(lang).or_insert_with_key(|k| labels_for(k));
        for m in find_mentions(&src.block.text, labels) {
            let Some((target_idx, confidence)) = resolve(blocks, &body, i, &m, labels) else { continue };
            let mut to = blocks[target_idx].block.block_hk.clone();
            // captions stand in for the figure/table they describe
            if blocks[target_idx].block.kind == BlockKind::Caption {
                if let Some(t) = caption_target.get(&to) {
                    to = (*t).clone();
                }
            }
            if to == src.block.block_hk {
                continue;
            }
            if seen.insert((src.block.block_hk.clone(), to.clone())) {
                links.push(Link { kind: LinkKind::RefersTo, from: src.block.block_hk.clone(), to, confidence, header_repeated: false });
            }
        }
    }
    doc.links.extend(links);
}

/// Confidence when the target is unique.
pub const CONFIDENCE_UNIQUE: f32 = 0.8;
/// Confidence when several blocks carried the number and the nearest was chosen.
pub const CONFIDENCE_AMBIGUOUS: f32 = 0.5;
/// Confidence when no formula carried the equation number and the nearest formula was chosen.
pub const CONFIDENCE_FALLBACK: f32 = 0.4;

/// Returns `(target block index, confidence)`.
fn resolve(blocks: &[DocBlock], body: &[usize], from: usize, m: &Mention, labels: &[RefLabel]) -> Option<(usize, f32)> {
    let candidates: Vec<usize> = match m.target {
        RefTarget::Figure | RefTarget::Table => {
            let captions: Vec<usize> = body
                .iter()
                .copied()
                .filter(|&j| j != from && blocks[j].block.kind == BlockKind::Caption)
                .filter(|&j| leading_label_number(&blocks[j].block.text, labels, m.target).is_some_and(|n| numbers_match(&m.number, &n)))
                .collect();
            if !captions.is_empty() {
                captions
            } else {
                // no caption block: a table/figure block whose own text starts with the label
                let wanted = if m.target == RefTarget::Table { &[BlockKind::Table][..] } else { &[BlockKind::Figure, BlockKind::Chart][..] };
                body.iter()
                    .copied()
                    .filter(|&j| j != from && wanted.contains(&blocks[j].block.kind))
                    .filter(|&j| leading_label_number(&blocks[j].block.text, labels, m.target).is_some_and(|n| numbers_match(&m.number, &n)))
                    .collect()
            }
        }
        RefTarget::Equation => {
            let formulas: Vec<usize> = body.iter().copied().filter(|&j| j != from && blocks[j].block.kind == BlockKind::Formula).collect();
            let numbered: Vec<usize> = formulas.iter().copied().filter(|&j| formula_number(&blocks[j].block).as_deref() == Some(m.number.as_str())).collect();
            if numbered.is_empty() {
                let (idx, _) = nearest(&formulas, from)?;
                return Some((idx, CONFIDENCE_FALLBACK));
            }
            numbered
        }
        RefTarget::Section => body
            .iter()
            .copied()
            .filter(|&j| j != from && matches!(blocks[j].block.kind, BlockKind::Heading | BlockKind::Title))
            .filter(|&j| heading_number(&blocks[j].block.text).as_deref() == Some(m.number.as_str()))
            .collect(),
    };
    let (idx, unique) = nearest(&candidates, from)?;
    Some((idx, if unique { CONFIDENCE_UNIQUE } else { CONFIDENCE_AMBIGUOUS }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mentions(text: &str, lang: &str) -> Vec<(RefTarget, String)> {
        find_mentions(text, &labels_for(lang)).into_iter().map(|m| (m.target, m.number)).collect()
    }

    #[test]
    fn english_mentions() {
        assert_eq!(
            mentions("As Table 3 and Tab. 4 show (see Fig. 2, Figure 2a), Eq. (4) and Equation 5 hold in Section 3.1 and Sec. 2.", "en"),
            vec![
                (RefTarget::Table, "3".into()),
                (RefTarget::Table, "4".into()),
                (RefTarget::Figure, "2".into()),
                (RefTarget::Figure, "2a".into()),
                (RefTarget::Equation, "4".into()),
                (RefTarget::Equation, "5".into()),
                (RefTarget::Section, "3.1".into()),
                (RefTarget::Section, "2".into()),
            ]
        );
    }

    #[test]
    fn word_boundaries_and_missing_numbers() {
        assert!(mentions("configure 3 tables", "en").is_empty());
        assert!(mentions("the table shows", "en").is_empty());
        assert!(mentions("Eq. (4", "en").is_empty());
        assert_eq!(mentions("§ 12 applies", "en"), vec![(RefTarget::Section, "12".into())]);
    }

    #[test]
    fn german_chinese_and_fallback() {
        assert_eq!(mentions("Siehe Abb. 2 und Tabelle 3, Gl. (7), Abschnitt 4.2.", "de"), vec![
            (RefTarget::Figure, "2".into()),
            (RefTarget::Table, "3".into()),
            (RefTarget::Equation, "7".into()),
            (RefTarget::Section, "4.2".into()),
        ]);
        // English labels still work on a German page
        assert_eq!(mentions("siehe Fig. 1", "de"), vec![(RefTarget::Figure, "1".into())]);
        assert_eq!(mentions("如图 2和表3所示，见第3节。", "zh"), vec![
            (RefTarget::Figure, "2".into()),
            (RefTarget::Table, "3".into()),
            (RefTarget::Section, "3".into()),
        ]);
        // unknown language → English
        assert_eq!(mentions("see Table 1", "xx"), vec![(RefTarget::Table, "1".into())]);
        assert!(mentions("Tabelle 1", "xx").is_empty());
    }

    #[test]
    fn caption_and_heading_numbers() {
        let en = labels_for("en");
        assert_eq!(leading_label_number("Table 3: Results", &en, RefTarget::Table), Some("3".into()));
        assert_eq!(leading_label_number("  Fig. 2. A plot", &en, RefTarget::Figure), Some("2".into()));
        assert_eq!(leading_label_number("Table 3", &en, RefTarget::Figure), None);
        assert_eq!(leading_label_number("Results in Table 3", &en, RefTarget::Table), None);
        assert_eq!(heading_number("3.1. Scope"), Some("3.1".into()));
        assert_eq!(heading_number("Introduction"), None);
        assert!(numbers_match("2a", "2"));
        assert!(!numbers_match("2", "2a"));
        assert!(!numbers_match("21", "2"));
    }
}
