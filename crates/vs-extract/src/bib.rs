//! Bibliography parsing and citation linking (frame `cites`).
//!
//! Reference-list blocks are detected by section title (References / Bibliography /
//! Literatur …), by `BlockKind::Reference`, or by text starting with a `[n]` marker. Each
//! entry becomes a `reference` entity; citation markers in body text (`[12]`, `[3,4]`,
//! `[1-3]`, `(Smith et al., 2024)`, `Smith et al. (2024)`) are linked with `cites`.

use std::collections::HashMap;

use vs_core::{BlockKind, Evidence};
use vs_stitch::StitchedDocument;

use crate::model::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, Fragment, Mention, Qualifiers};

const REFERENCE_TITLES: &[&str] = &[
    "references",
    "reference list",
    "bibliography",
    "literatur",
    "literaturverzeichnis",
    "quellen",
    "quellenverzeichnis",
    "works cited",
    "referencias",
    "références",
    "bibliographie",
    "bibliografia",
];

/// Section title without leading numbering/punctuation, lowercased.
fn normalised_title(title: &str) -> String {
    let t = title.trim().to_lowercase();
    let t = t.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ' || c == ')' || c == '(');
    t.trim_end_matches([':', '.']).trim().to_string()
}

pub fn is_reference_section_title(title: &str) -> bool {
    let t = normalised_title(title);
    REFERENCE_TITLES.contains(&t.as_str())
}

/// Leading `[n]` marker: returns `(n, chars consumed)`.
fn leading_marker(text: &str) -> Option<(u32, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if chars.get(i) != Some(&'[') {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j == start || chars.get(j) != Some(&']') || j - start > 4 {
        return None;
    }
    let n: u32 = chars[start..j].iter().collect::<String>().parse().ok()?;
    Some((n, j + 1))
}

/// Indices of body blocks that belong to the reference list.
pub fn reference_block_indices(doc: &StitchedDocument) -> Vec<usize> {
    let mut out = vec![];
    for s in doc.sections.iter().filter(|s| is_reference_section_title(&s.title)) {
        out.extend(s.body.iter().copied().filter(|&i| doc.is_body(i) && !matches!(doc.blocks[i].block.kind, BlockKind::Table | BlockKind::Figure | BlockKind::Chart)));
    }
    for (i, b) in doc.blocks.iter().enumerate() {
        if !doc.is_body(i) || out.contains(&i) {
            continue;
        }
        let kind_ok = matches!(b.block.kind, BlockKind::Reference | BlockKind::Paragraph | BlockKind::List | BlockKind::Other);
        if b.block.kind == BlockKind::Reference || (kind_ok && leading_marker(&b.block.text).is_some() && b.block.text.chars().count() > 20 && has_year(&b.block.text)) {
            out.push(i);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// One entry of a reference block: `(marker, char_start, char_end)` of the entry text.
#[derive(Debug, Clone, PartialEq)]
pub struct EntrySpan {
    pub marker: Option<u32>,
    pub char_start: usize,
    pub char_end: usize,
}

fn has_year(s: &str) -> bool {
    find_year(s).is_some()
}

/// Finds the first plausible publication year `(year, char index)`.
pub fn find_year(s: &str) -> Option<(u16, usize)> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 4 <= chars.len() {
        let before_ok = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        let after_ok = i + 4 == chars.len() || !chars[i + 4].is_ascii_digit();
        if before_ok && after_ok && chars[i..i + 4].iter().all(|c| c.is_ascii_digit()) {
            let y: u16 = chars[i..i + 4].iter().collect::<String>().parse().unwrap_or(0);
            if (1800..=2099).contains(&y) {
                return Some((y, i));
            }
        }
        i += 1;
    }
    None
}

/// Splits a reference block into entries. With `[n]` markers, entries start at each marker at a
/// line start (or after a sentence end). Without markers, lines are merged until a chunk has a
/// year and the next line starts a new entry (uppercase letter).
pub fn split_entries(text: &str) -> Vec<EntrySpan> {
    let chars: Vec<char> = text.chars().collect();
    let mut starts: Vec<(usize, u32)> = vec![];
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            let rest: String = chars[i..chars.len().min(i + 8)].iter().collect();
            if let Some((n, _)) = leading_marker(&rest) {
                let prev = chars[..i].iter().rev().find(|c| !c.is_whitespace() || **c == '\n');
                let at_boundary = match prev {
                    None => true,
                    Some('\n') | Some('.') => true,
                    _ => false,
                };
                // Also accept markers after a newline that was swallowed as a space.
                let prev_is_newline = i > 0 && chars[i - 1] == '\n';
                if at_boundary || prev_is_newline {
                    starts.push((i, n));
                }
            }
        }
        i += 1;
    }
    let mut out = vec![];
    if !starts.is_empty() {
        for (k, &(s, n)) in starts.iter().enumerate() {
            let e = starts.get(k + 1).map(|x| x.0).unwrap_or(chars.len());
            let (s2, e2) = trim_range(&chars, s, e);
            if e2 > s2 {
                out.push(EntrySpan { marker: Some(n), char_start: s2, char_end: e2 });
            }
        }
        return out;
    }
    // No markers: merge lines.
    let mut line_starts = vec![0usize];
    for (k, c) in chars.iter().enumerate() {
        if *c == '\n' {
            line_starts.push(k + 1);
        }
    }
    let mut chunk_start = 0usize;
    for (k, &ls) in line_starts.iter().enumerate() {
        let le = line_starts.get(k + 1).map(|x| x - 1).unwrap_or(chars.len());
        let chunk: String = chars[chunk_start..le].iter().collect();
        let next_starts_entry = match line_starts.get(k + 1) {
            Some(&n) => starts_entry(&chars[n..].iter().collect::<String>()),
            None => true,
        };
        let _ = ls;
        if (has_year(&chunk) && chunk.trim_end().ends_with('.') && next_starts_entry) || k + 1 == line_starts.len() {
            let (s2, e2) = trim_range(&chars, chunk_start, le);
            if e2 > s2 {
                out.push(EntrySpan { marker: None, char_start: s2, char_end: e2 });
            }
            chunk_start = line_starts.get(k + 1).copied().unwrap_or(chars.len());
        }
    }
    out
}

/// Does a line look like the start of a reference entry? `[n]`, a number, `Surname,` or an
/// initial (`J.`) as the first token.
fn starts_entry(line: &str) -> bool {
    let line = line.trim_start();
    let Some(first) = line.split_whitespace().next() else { return false };
    let c = first.chars().next().unwrap();
    c == '[' || c.is_ascii_digit() || (c.is_uppercase() && (first.ends_with(',') || is_initial(first)))
}

fn trim_range(chars: &[char], mut s: usize, mut e: usize) -> (usize, usize) {
    while s < e && chars[s].is_whitespace() {
        s += 1;
    }
    while e > s && chars[e - 1].is_whitespace() {
        e -= 1;
    }
    (s, e)
}

/// Fields recovered from one reference entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedReference {
    pub marker: Option<u32>,
    pub first_author: Option<String>,
    pub year: Option<u16>,
    pub title: String,
    pub doi: Option<String>,
    pub raw: String,
}

fn is_initial(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| c == ',' || c == ';');
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    t.ends_with('.') && letters.len() <= 2 && !letters.is_empty() && letters.iter().all(|c| c.is_uppercase())
}

/// Splits on `. ` boundaries that are not initials or common abbreviations.
fn sentence_segments(s: &str) -> Vec<String> {
    const ABBREV: &[&str] = &["al", "et al", "eds", "ed", "vol", "pp", "no", "proc", "conf", "int", "j", "trans", "ieee", "acm", "jr", "st"];
    let mut segs = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        cur.push(chars[i]);
        if chars[i] == '.' && (i + 1 == chars.len() || chars[i + 1].is_whitespace()) {
            let last_raw: &str = cur.trim_end_matches('.').rsplit(|c: char| c.is_whitespace() || c == ',').next().unwrap_or("");
            let last_word = last_raw.to_lowercase();
            let letters: Vec<char> = last_raw.chars().filter(|c| c.is_alphabetic()).collect();
            let initial = letters.len() <= 2 && letters.iter().all(|c| c.is_uppercase()) && last_raw.chars().all(|c| c.is_alphabetic() || c == '.' || c == '-');
            let next_upper = chars.get(i + 2).is_some_and(|c| c.is_uppercase());
            // "et al." ends an author list when a capitalised title follows.
            let abbrev = (ABBREV.contains(&last_word.as_str()) && !(last_word == "al" && next_upper)) || initial;
            if !abbrev {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    segs.push(t);
                }
                cur.clear();
            }
        }
        i += 1;
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        segs.push(t);
    }
    segs
}

fn clean_title(s: &str) -> String {
    s.trim()
        .trim_matches(|c: char| c == '"' || c == '“' || c == '”' || c == '\'' || c == '‘' || c == '’' || c == '.' || c == ',' || c == ' ')
        .to_string()
}

fn first_surname(authors: &str) -> Option<String> {
    let cleaned: String = authors.chars().map(|c| if c == '(' || c == ')' { ' ' } else { c }).collect();
    for tok in cleaned.split_whitespace() {
        let t = tok.trim_matches(|c: char| c == ',' || c == ';' || c == ':');
        let lower = t.to_lowercase();
        if t.is_empty() || is_initial(t) || matches!(lower.as_str(), "and" | "&" | "et" | "al." | "al" | "und") {
            continue;
        }
        if t.chars().next().is_some_and(|c| c.is_uppercase()) && t.chars().filter(|c| c.is_alphabetic()).count() >= 2 && !t.chars().any(|c| c.is_ascii_digit()) {
            return Some(t.trim_end_matches('.').to_string());
        }
    }
    None
}

/// Heuristic parse of one entry into author / year / title / DOI.
pub fn parse_entry(entry: &str) -> ParsedReference {
    let (marker, body) = match leading_marker(entry) {
        Some((n, used)) => (Some(n), entry.chars().skip(used).collect::<String>()),
        None => (None, entry.to_string()),
    };
    let body = body.trim().to_string();
    let doi = body
        .split(|c: char| c.is_whitespace() || c == '<' || c == '>')
        .find_map(|t| {
            let t = t.trim_end_matches(['.', ',', ';']);
            let t = t.strip_prefix("https://doi.org/").or_else(|| t.strip_prefix("http://doi.org/")).or_else(|| t.strip_prefix("doi:")).unwrap_or(t);
            (t.starts_with("10.") && t.contains('/')).then(|| t.to_string())
        });
    let year_pos = find_year(&body);
    let segs = sentence_segments(&body);
    let (first_author, title) = if segs.len() < 2 {
        (first_surname(&body), clean_title(year_pos.map(|(_, i)| body.chars().take(i).collect::<String>()).as_deref().unwrap_or(&body)))
    } else {
        // Which segment holds the year? Early year (author-year style) → title follows it;
        // otherwise authors are segment 0 and the title is segment 1.
        let mut acc = 0usize;
        let mut year_seg = None;
        for (k, s) in segs.iter().enumerate() {
            let len = s.chars().count();
            if let Some((_, yp)) = year_pos {
                if yp < acc + len + 1 {
                    year_seg = Some(k);
                    break;
                }
            }
            acc += len + 1;
        }
        match year_seg {
            Some(k) if k == 0 || (k == 1 && segs[1].chars().count() <= 8) => {
                let authors: String = segs[..=k].join(" ");
                let title = segs.get(k + 1).map(|s| clean_title(s)).unwrap_or_default();
                (first_surname(&authors), title)
            }
            _ => (first_surname(&segs[0]), clean_title(&segs[1])),
        }
    };
    let title = if title.is_empty() { clean_title(&body) } else { title };
    ParsedReference { marker, first_author, year: year_pos.map(|(y, _)| y), title, doi, raw: body }
}

/// Alias forms a body text may use to cite this reference.
pub fn reference_aliases(r: &ParsedReference) -> Vec<String> {
    let mut out = vec![];
    if let Some(n) = r.marker {
        out.push(format!("[{n}]"));
    }
    if let (Some(a), Some(y)) = (&r.first_author, r.year) {
        out.push(format!("{a} et al., {y}"));
        out.push(format!("{a} et al. ({y})"));
        out.push(format!("{a}, {y}"));
        out.push(format!("{a} ({y})"));
    }
    if let Some(d) = &r.doi {
        out.push(format!("doi:{d}"));
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum CitationKey {
    Number(u32),
    AuthorYear { surname: String, year: u16 },
}

/// A citation marker found in body text (char offsets).
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub char_start: usize,
    pub char_end: usize,
    pub keys: Vec<CitationKey>,
}

fn parse_numeric_group(inner: &str) -> Option<Vec<u32>> {
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '-' || c == '–' || c == ' ' || c == ';') {
        return None;
    }
    let mut nums = vec![];
    for part in inner.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let range: Vec<&str> = part.split(['-', '–']).map(str::trim).collect();
        match range.as_slice() {
            [a] => nums.push(a.parse::<u32>().ok()?),
            [a, b] => {
                let (a, b) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
                if b < a || b - a > 50 {
                    return None;
                }
                nums.extend(a..=b);
            }
            _ => return None,
        }
    }
    (!nums.is_empty() && nums.iter().all(|n| *n > 0 && *n < 10_000)).then_some(nums)
}

/// Author-year inside parentheses: `Smith et al., 2024`, `Smith and Doe 2024`, `Smith, 2024a`.
fn parse_author_year_part(part: &str) -> Option<CitationKey> {
    let (year, yp) = find_year(part)?;
    let before: String = part.chars().take(yp).collect();
    let before = before.trim().trim_end_matches([',', ';', ' ']);
    // "see Smith et al." → drop leading lowercase words
    let surname = first_surname(before)?;
    if before.chars().count() > 80 {
        return None;
    }
    Some(CitationKey::AuthorYear { surname, year })
}

/// Finds citation markers in `text`. Char offsets refer to `text`.
pub fn find_citations(text: &str) -> Vec<Citation> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        if open != '[' && open != '(' {
            i += 1;
            continue;
        }
        let close = if open == '[' { ']' } else { ')' };
        let Some(rel) = chars[i + 1..].iter().position(|c| *c == close || *c == open) else { break };
        let j = i + 1 + rel;
        if chars[j] != close {
            i = j;
            continue;
        }
        let inner: String = chars[i + 1..j].iter().collect();
        if open == '[' {
            if let Some(nums) = parse_numeric_group(inner.trim()) {
                out.push(Citation { char_start: i, char_end: j + 1, keys: nums.into_iter().map(CitationKey::Number).collect() });
            }
        } else {
            let trimmed = inner.trim();
            // Narrative form: "Smith et al. (2024)" — the parentheses hold only a year.
            if trimmed.chars().count() <= 5 && find_year(trimmed).is_some_and(|(_, p)| p == 0) {
                if let Some((start, key)) = narrative_author(&chars, i, find_year(trimmed).unwrap().0) {
                    out.push(Citation { char_start: start, char_end: j + 1, keys: vec![key] });
                }
            } else {
                let keys: Vec<CitationKey> = trimmed.split(';').filter_map(parse_author_year_part).collect();
                if !keys.is_empty() {
                    out.push(Citation { char_start: i, char_end: j + 1, keys });
                }
            }
        }
        i = j + 1;
    }
    out
}

/// Looks backwards from `(` at `paren` for `Surname et al.` / `Surname and Doe` / `Surname`.
fn narrative_author(chars: &[char], paren: usize, year: u16) -> Option<(usize, CitationKey)> {
    let before: String = chars[..paren].iter().collect();
    let trimmed = before.trim_end();
    let mut words: Vec<(usize, &str)> = vec![];
    let mut idx = trimmed.len();
    for w in trimmed.rsplit(' ').take(5) {
        idx -= w.len();
        words.push((idx, w));
        idx = idx.saturating_sub(1);
    }
    // words: nearest first
    let mut k = 0;
    let et_al = words.first().map(|w| w.1) == Some("al.") && words.get(1).map(|w| w.1) == Some("et");
    let and_coauthor = matches!(words.get(1).map(|w| w.1), Some("and") | Some("&"));
    if et_al || and_coauthor {
        k += 2;
    }
    let (byte_start, surname) = words.get(k)?;
    let surname = surname.trim_matches(|c: char| c == ',' || c == '(');
    if !surname.chars().next().is_some_and(|c| c.is_uppercase()) || surname.chars().count() < 2 {
        return None;
    }
    let char_start = trimmed[..*byte_start].chars().count();
    Some((char_start, CitationKey::AuthorYear { surname: surname.to_string(), year }))
}

/// Parses the reference list and links citations. Returns the fragment and a map
/// `reference local_id → ParsedReference` for callers that want the parsed fields.
pub fn extract_bibliography(doc: &StitchedDocument, provenance_idx: usize) -> (Fragment, Vec<(String, ParsedReference)>) {
    let mut frag = Fragment::default();
    let mut parsed: Vec<(String, ParsedReference)> = vec![];
    let ref_blocks = reference_block_indices(doc);
    let mut by_number: HashMap<u32, String> = HashMap::new();
    let mut by_author_year: HashMap<(String, u16), String> = HashMap::new();
    let mut seq = 0u32;
    let mut m = 0usize;
    for &i in &ref_blocks {
        let b = &doc.blocks[i].block;
        let chars: Vec<char> = b.text.chars().collect();
        for span in split_entries(&b.text) {
            let entry: String = chars[span.char_start..span.char_end].iter().collect();
            if entry.chars().count() < 15 {
                continue;
            }
            seq += 1;
            let mut r = parse_entry(&entry);
            match r.marker {
                Some(n) => seq = n,
                None => r.marker = Some(seq),
            }
            let local_id = format!("bib.r{}", parsed.len() + 1);
            m += 1;
            let mention_id = format!("bib.m{m}");
            frag.mentions.push(Mention {
                id: mention_id.clone(),
                block_hk: b.block_hk.clone(),
                char_start: span.char_start as u32,
                char_end: span.char_end as u32,
                surface: entry.clone(),
                raw_type: "reference".into(),
                confidence: 0.8,
            });
            let aliases = reference_aliases(&r);
            frag.entities.push(DocEntity {
                local_id: local_id.clone(),
                name: r.title.clone(),
                raw_type: "reference".into(),
                type_id: None,
                aliases,
                mentions: vec![mention_id],
                entity_hk: None,
            });
            if let Some(n) = r.marker {
                by_number.entry(n).or_insert_with(|| local_id.clone());
            }
            if let (Some(a), Some(y)) = (&r.first_author, r.year) {
                by_author_year.entry((a.to_lowercase(), y)).or_insert_with(|| local_id.clone());
            }
            parsed.push((local_id, r));
        }
    }
    if parsed.is_empty() {
        return (frag, parsed);
    }

    let mut c = 0usize;
    for (i, b) in doc.blocks.iter().enumerate() {
        if !doc.is_body(i) || ref_blocks.contains(&i) || !matches!(b.block.kind, BlockKind::Paragraph | BlockKind::List | BlockKind::Caption | BlockKind::Footnote | BlockKind::Other) {
            continue;
        }
        for cit in find_citations(&b.block.text) {
            let (confidence, keys) = (if cit.keys.iter().any(|k| matches!(k, CitationKey::Number(_))) { 0.9 } else { 0.8 }, cit.keys);
            for key in keys {
                let target = match &key {
                    CitationKey::Number(n) => by_number.get(n),
                    CitationKey::AuthorYear { surname, year } => by_author_year.get(&(surname.to_lowercase(), *year)),
                };
                let Some(target) = target else { continue };
                c += 1;
                frag.assertions.push(Assertion {
                    local_id: format!("bib.c{c}"),
                    predicate: "cites".into(),
                    predicate_id: None,
                    kind: AssertionKind::Nary,
                    args: vec![
                        Arg { role: "claim_block".into(), value: ArgValue::Literal { value: b.block.block_hk.to_string(), unit: None } },
                        Arg { role: "reference".into(), value: ArgValue::Entity { local_id: target.clone() } },
                    ],
                    qualifiers: Qualifiers::default(),
                    evidence: vec![Evidence::Span { block_hk: b.block.block_hk.clone(), char_start: cit.char_start as u32, char_end: cit.char_end as u32 }],
                    confidence,
                    verified: false,
                    provenance_idx,
                });
            }
        }
    }
    (frag, parsed)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::DocBuilder;

    #[test]
    fn detects_reference_sections_and_marker_blocks() {
        assert!(is_reference_section_title("6. References"));
        assert!(is_reference_section_title("Literaturverzeichnis"));
        assert!(!is_reference_section_title("6. Results"));
        let mut b = DocBuilder::new();
        b.block(BlockKind::Heading, "1 Intro");
        b.block(BlockKind::Paragraph, "[1] is cited here but this is body text, short.");
        b.block(BlockKind::Heading, "References");
        b.block(BlockKind::Paragraph, "[1] J. Smith. A title. 2020.");
        b.block(BlockKind::Heading, "Appendix");
        b.block(BlockKind::Paragraph, "[7] A. Doe and B. Roe. Another long title here. In Proc. X, 2019.");
        b.block(BlockKind::Reference, "Plain reference block");
        assert_eq!(reference_block_indices(&b.doc), vec![3, 5, 6]);
    }

    #[test]
    fn splits_marked_entries() {
        let t = "[1] A. Author. First title. 2020.\n[2] B. Author. Second title. 2021. [3] C. Author. Third. 2022.";
        let spans = split_entries(t);
        assert_eq!(spans.iter().map(|s| s.marker).collect::<Vec<_>>(), vec![Some(1), Some(2), Some(3)]);
        let chars: Vec<char> = t.chars().collect();
        let second: String = chars[spans[1].char_start..spans[1].char_end].iter().collect();
        assert_eq!(second, "[2] B. Author. Second title. 2021.");
    }

    #[test]
    fn splits_unmarked_entries_by_year_and_line() {
        let t = "Smith, J. (2020). A study of things.\nJournal of Stuff, 1(2).\nDoe, A. (2019). Another study. Proc. Y.";
        let spans = split_entries(t);
        assert_eq!(spans.len(), 2);
        let chars: Vec<char> = t.chars().collect();
        let first: String = chars[spans[0].char_start..spans[0].char_end].iter().collect();
        assert!(first.ends_with("1(2)."));
    }

    #[test]
    fn parses_author_year_style() {
        let r = parse_entry("[4] Smith, J., & Doe, A. (2020). Attention is all you need. In NeurIPS, pages 1–10. https://doi.org/10.1000/xyz123.");
        assert_eq!(r.marker, Some(4));
        assert_eq!(r.first_author.as_deref(), Some("Smith"));
        assert_eq!(r.year, Some(2020));
        assert_eq!(r.title, "Attention is all you need");
        assert_eq!(r.doi.as_deref(), Some("10.1000/xyz123"));
        let aliases = reference_aliases(&r);
        assert!(aliases.contains(&"[4]".to_string()));
        assert!(aliases.contains(&"Smith et al., 2020".to_string()));
    }

    #[test]
    fn parses_numeric_style() {
        let r = parse_entry("[12] J. Devlin, M.-W. Chang, K. Lee, and K. Toutanova. BERT: Pre-training of deep bidirectional transformers. In Proc. of NAACL, pages 4171–4186, 2019.");
        assert_eq!(r.first_author.as_deref(), Some("Devlin"));
        assert_eq!(r.year, Some(2019));
        assert_eq!(r.title, "BERT: Pre-training of deep bidirectional transformers");
    }

    #[test]
    fn finds_numeric_and_author_year_citations() {
        let t = "As shown in [3, 5-7] and (Smith et al., 2020; Doe 2019), Roe et al. (2018) agree. Values (12 %) are not citations [x].";
        let cits = find_citations(t);
        assert_eq!(cits.len(), 3);
        assert_eq!(cits[0].keys, vec![3, 5, 6, 7].into_iter().map(CitationKey::Number).collect::<Vec<_>>());
        assert_eq!(
            cits[1].keys,
            vec![
                CitationKey::AuthorYear { surname: "Smith".into(), year: 2020 },
                CitationKey::AuthorYear { surname: "Doe".into(), year: 2019 }
            ]
        );
        assert_eq!(cits[2].keys, vec![CitationKey::AuthorYear { surname: "Roe".into(), year: 2018 }]);
        let chars: Vec<char> = t.chars().collect();
        let surface: String = chars[cits[2].char_start..cits[2].char_end].iter().collect();
        assert_eq!(surface, "Roe et al. (2018)");
    }

    #[test]
    fn links_citations_to_references() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Heading, "1 Intro");
        let body = b.block(BlockKind::Paragraph, "Transformers [2] and BERT (Devlin et al., 2019) dominate; see also [9].");
        b.block(BlockKind::Heading, "References");
        b.block(
            BlockKind::Paragraph,
            "[1] J. Devlin, M.-W. Chang, K. Lee, and K. Toutanova. BERT: Pre-training of deep bidirectional transformers. In NAACL, 2019.\n[2] A. Vaswani et al. Attention is all you need. In NeurIPS, 2017.",
        );
        let (frag, parsed) = extract_bibliography(&b.doc, 0);
        assert_eq!(parsed.len(), 2);
        assert_eq!(frag.entities.len(), 2);
        assert_eq!(frag.entities[1].name, "Attention is all you need");
        assert_eq!(frag.mentions.len(), 2);
        // [2] → Vaswani, (Devlin et al., 2019) → Devlin; [9] has no target
        assert_eq!(frag.assertions.len(), 2);
        let targets: Vec<&str> = frag
            .assertions
            .iter()
            .map(|a| match &a.args[1].value {
                ArgValue::Entity { local_id } => local_id.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(targets, vec!["bib.r2", "bib.r1"]);
        assert_eq!(frag.assertions[0].args[0], Arg { role: "claim_block".into(), value: ArgValue::Literal { value: b.hk(body).to_string(), unit: None } });
        assert!(matches!(&frag.assertions[0].evidence[0], Evidence::Span { char_start: 13, char_end: 16, .. }));
    }
}
