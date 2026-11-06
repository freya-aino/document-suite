//! Entity resolution and type normalisation.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use vs_core::{HashKey, TenantId};
use vs_stitch::StitchedDocument;

use crate::model::{ArgValue, DocEntity, ExtractionOutput};
use crate::ExtractError;

/// Finds abbreviation definitions of the form `Long Form (LF)`. Function words in the long
/// form (`of`, `from`, `and`, …) do not need an initial. Returns `(long_form, short_form)` pairs.
pub fn abbreviation_definitions(text: &str) -> Vec<(String, String)> {
    const STOP: &[&str] = &["of", "from", "for", "and", "the", "in", "on", "with", "to", "by", "a", "an", "de", "der", "die", "das", "des", "und", "für", "von", "du", "la", "le", "et"];
    let mut out = vec![];
    let mut rest = text;
    let mut offset = 0;
    while let Some(open) = rest.find('(') {
        let Some(close_rel) = rest[open..].find(')') else { break };
        let short = rest[open + 1..open + close_rel].trim();
        let before = &text[..offset + open];
        if (2..=10).contains(&short.chars().count()) && short.chars().any(|c| c.is_uppercase()) && !short.contains(' ') {
            let initials: Vec<char> = short.chars().filter(|c| c.is_alphabetic()).map(|c| c.to_ascii_lowercase()).collect();
            let words: Vec<&str> = before.split_whitespace().collect();
            if !initials.is_empty() {
                // Walk backwards over words, consuming initials from the end; skip stopwords.
                let mut k = initials.len();
                let mut w = words.len();
                let mut ok = true;
                while k > 0 {
                    if w == 0 {
                        ok = false;
                        break;
                    }
                    let word = words[w - 1];
                    let first = word.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_ascii_lowercase());
                    if first == Some(initials[k - 1]) {
                        k -= 1;
                        w -= 1;
                    } else if STOP.contains(&word.to_lowercase().as_str()) && w < words.len() {
                        w -= 1;
                    } else {
                        ok = false;
                        break;
                    }
                }
                if ok && w < words.len() {
                    out.push((words[w..].join(" "), short.to_string()));
                }
            }
        }
        offset += open + close_rel + 1;
        rest = &text[offset..];
    }
    out
}

/// Lowercase slug: runs of non-alphanumerics become `_`; empty → `unknown`.
pub fn slug(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = true;
    for c in raw.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
    }
    let out = out.trim_end_matches('_').to_string();
    if out.is_empty() { "unknown".to_string() } else { out }
}

/// Name key for in-document matching: lowercase, whitespace collapsed, surrounding punctuation
/// and a leading article removed.
pub fn normalise_name(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let stripped = lower.trim_matches(|c: char| !c.is_alphanumeric());
    let words: Vec<&str> = stripped.split_whitespace().collect();
    let words = match words.as_slice() {
        [first, rest @ ..] if !rest.is_empty() && matches!(*first, "the" | "a" | "an") => rest.to_vec(),
        w => w.to_vec(),
    };
    words.join(" ")
}

const GENERIC_TYPES: &[&str] = &["", "unknown", "entity", "other", "thing", "concept", "term", "misc", "object"];

fn specific_type(raw: &str) -> Option<String> {
    let s = slug(raw);
    (!GENERIC_TYPES.contains(&s.as_str()) && !GENERIC_TYPES.contains(&raw.trim())).then_some(s)
}

/// Merges entities across windows: same normalised name (abbreviation definitions join both
/// forms) and compatible raw type (equal, or one of them generic). Aliases are kept; assertion
/// arguments are rewritten to the surviving `local_id`.
pub fn resolve_in_document(out: &mut ExtractionOutput, doc: &StitchedDocument) {
    // short form → long form, both normalised
    let mut abbrev: HashMap<String, String> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        if doc.is_body(i) {
            for (long, short) in abbreviation_definitions(&b.block.text) {
                abbrev.entry(normalise_name(&short)).or_insert_with(|| normalise_name(&long));
            }
        }
    }
    let key_of = |name: &str| {
        let k = normalise_name(name);
        abbrev.get(&k).cloned().unwrap_or(k)
    };

    struct Group {
        key: String,
        ty: Option<String>,
        members: Vec<usize>,
    }
    let mut groups: Vec<Group> = vec![];
    for (i, e) in out.entities.iter().enumerate() {
        let key = key_of(&e.name);
        if key.is_empty() {
            groups.push(Group { key, ty: specific_type(&e.raw_type), members: vec![i] });
            continue;
        }
        let ty = specific_type(&e.raw_type);
        let found = groups.iter_mut().find(|g| g.key == key && (g.ty.is_none() || ty.is_none() || g.ty == ty));
        match found {
            Some(g) => {
                g.members.push(i);
                if g.ty.is_none() {
                    g.ty = ty;
                }
            }
            None => groups.push(Group { key, ty, members: vec![i] }),
        }
    }

    let mut rename: HashMap<String, String> = HashMap::new();
    let mut merged: Vec<DocEntity> = Vec::with_capacity(groups.len());
    for g in groups {
        let first = &out.entities[g.members[0]];
        let mut survivor = first.clone();
        // Prefer the long form of an abbreviation as the name; else the first seen.
        if let Some(long) = g.members.iter().map(|&i| &out.entities[i]).find(|e| normalise_name(&e.name) == g.key && abbrev.values().any(|v| *v == g.key)) {
            survivor.name = long.name.clone();
        }
        if let Some(ty) = g.ty.as_ref() {
            if specific_type(&survivor.raw_type).is_none() {
                survivor.raw_type = g.members.iter().map(|&i| &out.entities[i]).find(|e| specific_type(&e.raw_type).as_ref() == Some(ty)).map(|e| e.raw_type.clone()).unwrap_or(survivor.raw_type);
            }
        }
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(survivor.name.to_lowercase());
        let mut aliases = vec![];
        let mut mentions = vec![];
        for &i in &g.members {
            let e = &out.entities[i];
            rename.insert(e.local_id.clone(), survivor.local_id.clone());
            for alias in std::iter::once(&e.name).chain(e.aliases.iter()) {
                if seen.insert(alias.to_lowercase()) {
                    aliases.push(alias.clone());
                }
            }
            mentions.extend(e.mentions.iter().cloned());
            if survivor.entity_hk.is_none() {
                survivor.entity_hk = e.entity_hk.clone();
            }
        }
        mentions.dedup();
        survivor.aliases = aliases;
        survivor.mentions = mentions;
        survivor.type_id = None;
        merged.push(survivor);
    }
    out.entities = merged;
    for a in &mut out.assertions {
        for arg in &mut a.args {
            if let ArgValue::Entity { local_id } = &mut arg.value {
                if let Some(n) = rename.get(local_id) {
                    *local_id = n.clone();
                }
            }
        }
    }
}

/// Maps open types/predicates to the ontology; creates pending types when nothing matches.
#[async_trait]
pub trait TypeNormalizer: Send + Sync {
    async fn entity_type(&self, raw: &str) -> Result<String, ExtractError>;
    async fn predicate(&self, raw: &str) -> Result<String, ExtractError>;
    /// Like `predicate`, with the roles seen for this predicate (used when creating a pending
    /// relation type). Default: ignores roles.
    async fn predicate_with_roles(&self, raw: &str, _roles: &[String]) -> Result<String, ExtractError> {
        self.predicate(raw).await
    }
}

/// `type_id = slug(raw)`; no ontology. For tests and the fast profile.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTypeNormalizer;

#[async_trait]
impl TypeNormalizer for NoopTypeNormalizer {
    async fn entity_type(&self, raw: &str) -> Result<String, ExtractError> {
        Ok(slug(raw))
    }
    async fn predicate(&self, raw: &str) -> Result<String, ExtractError> {
        Ok(slug(raw))
    }
}

/// Decision for one document entity against the corpus.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Link { entity_hk: HashKey, score: f32 },
    New { entity_hk: HashKey },
}

/// Cross-document canonicalisation against HelixDB (implemented in vs-helix / vs-worker).
#[async_trait]
pub trait CorpusResolver: Send + Sync {
    async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, context: &str) -> Result<Resolution, ExtractError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arg, Assertion, AssertionKind, Qualifiers};
    use crate::test_util::DocBuilder;
    use vs_core::BlockKind;

    fn ent(id: &str, name: &str, ty: &str, mention: &str) -> DocEntity {
        DocEntity { local_id: id.into(), name: name.into(), raw_type: ty.into(), type_id: None, aliases: vec![], mentions: vec![mention.into()], entity_hk: None }
    }

    #[test]
    fn slug_and_name_keys() {
        assert_eq!(slug("  Machine-Learning Method! "), "machine_learning_method");
        assert_eq!(slug("??"), "unknown");
        assert_eq!(normalise_name("The  GLUE Benchmark."), "glue benchmark");
        assert_eq!(normalise_name("A"), "a");
    }

    #[test]
    fn merges_across_windows_with_abbreviations_and_types() {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Paragraph, "We use Optical Character Recognition (OCR) everywhere.");
        let mut out = ExtractionOutput { entities: vec![
            ent("w0.e1", "Optical Character Recognition", "task", "m1"),
            ent("w1.e1", "OCR", "", "m2"),
            ent("w1.e2", "bert", "method", "m3"),
            ent("w2.e1", "BERT", "Method", "m4"),
            ent("w2.e2", "BERT", "person", "m5"),
            ent("w2.e3", "the GLUE benchmark", "dataset", "m6"),
            ent("w3.e1", "GLUE benchmark", "entity", "m7"),
        ], ..Default::default() };
        out.assertions.push(Assertion {
            local_id: "a".into(),
            predicate: "uses".into(),
            predicate_id: None,
            kind: AssertionKind::Binary,
            args: vec![
                Arg { role: "subject".into(), value: ArgValue::Entity { local_id: "w2.e1".into() } },
                Arg { role: "object".into(), value: ArgValue::Entity { local_id: "w1.e1".into() } },
            ],
            qualifiers: Qualifiers::default(),
            evidence: vec![],
            confidence: 1.0,
            verified: false,
            provenance_idx: 0,
        });
        resolve_in_document(&mut out, &b.doc);
        let names: Vec<&str> = out.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Optical Character Recognition", "bert", "BERT", "the GLUE benchmark"]);
        assert_eq!(out.entities[0].aliases, vec!["OCR"]);
        assert_eq!(out.entities[0].mentions, vec!["m1", "m2"]);
        assert_eq!(out.entities[1].mentions, vec!["m3", "m4"]);
        assert!(out.entities[1].aliases.is_empty()); // case variants are not aliases
        assert_eq!(out.entities[2].raw_type, "person");
        assert_eq!(out.entities[3].mentions, vec!["m6", "m7"]);
        assert_eq!(out.entities[3].aliases, vec!["GLUE benchmark"]);
        assert_eq!(out.assertions[0].args[0].value, ArgValue::Entity { local_id: "w1.e2".into() });
        assert_eq!(out.assertions[0].args[1].value, ArgValue::Entity { local_id: "w0.e1".into() });
    }

    #[test]
    fn finds_abbreviations() {
        let t = "We use Optical Character Recognition (OCR) and a Vision Language Model (VLM). See (Fig. 2).";
        let defs = abbreviation_definitions(t);
        assert_eq!(defs, vec![
            ("Optical Character Recognition".to_string(), "OCR".to_string()),
            ("Vision Language Model".to_string(), "VLM".to_string()),
        ]);
        let t = "Bidirectional Encoder Representations from Transformers (BERT) and the University of California (UC).";
        let defs = abbreviation_definitions(t);
        assert_eq!(defs, vec![
            ("Bidirectional Encoder Representations from Transformers".to_string(), "BERT".to_string()),
            ("University of California".to_string(), "UC".to_string()),
        ]);
    }
}
