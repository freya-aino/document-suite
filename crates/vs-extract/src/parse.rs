//! Parsing the extraction model's JSON into the output model, with grounding.

use std::collections::HashMap;

use serde::Deserialize;
use vs_core::{Evidence, HashKey};

use crate::ground::locate_span;
use crate::model::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, Fragment, Mention, Qualifiers};
use crate::ExtractError;

/// The model's window output (lenient: optional fields default so non-strict servers work).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawOutput {
    #[serde(default)]
    pub entities: Vec<RawEntity>,
    #[serde(default)]
    pub assertions: Vec<RawAssertion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawQuote {
    pub block: String,
    pub quote: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawEntity {
    pub id: String,
    #[serde(default, alias = "surface")]
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub mentions: Vec<RawQuote>,
    /// Legacy single-mention form.
    #[serde(default)]
    pub block: Option<String>,
    #[serde(default)]
    pub quote: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawLiteral {
    Obj { value: serde_json::Value, #[serde(default)] unit: Option<String> },
    Scalar(serde_json::Value),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawArg {
    pub role: String,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub literal: Option<RawLiteral>,
    /// Legacy flat form.
    #[serde(default)]
    pub unit: Option<String>,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawAssertion {
    pub predicate: String,
    #[serde(default)]
    pub kind: Option<AssertionKind>,
    #[serde(default)]
    pub args: Vec<RawArg>,
    #[serde(default)]
    pub qualifiers: Qualifiers,
    #[serde(default)]
    pub evidence: Vec<RawQuote>,
    #[serde(default = "one")]
    pub confidence: f32,
}

/// Parses model text as `RawOutput`, tolerating a Markdown code fence.
pub fn parse_raw(text: &str) -> Result<RawOutput, ExtractError> {
    let t = text.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    serde_json::from_str(t).map_err(|e| ExtractError::Schema(format!("extraction output: {e}")))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Grounded {
    pub fragment: Fragment,
    pub dropped_ungrounded: u32,
}

fn literal_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Locates `quote` in the block behind `label`, then narrows to `focus` (e.g. the entity name)
/// inside the located text when possible. Returns `(block_hk, start, end, surface)`.
fn ground_quote(label: &str, quote: &str, focus: Option<&str>, labels: &HashMap<String, HashKey>, texts: &HashMap<HashKey, &str>) -> Option<(HashKey, u32, u32, String)> {
    let hk = labels.get(label.trim()).or_else(|| labels.get(label.trim().trim_start_matches('[').trim_end_matches(']')))?;
    let text = texts.get(hk)?;
    let (s, e) = locate_span(text, quote)?;
    let located: String = text.chars().skip(s as usize).take((e - s) as usize).collect();
    if let Some(f) = focus {
        if let Some((fs, fe)) = locate_span(&located, f) {
            let surface: String = located.chars().skip(fs as usize).take((fe - fs) as usize).collect();
            return Some((hk.clone(), s + fs, s + fe, surface));
        }
    }
    Some((hk.clone(), s, e, located))
}

/// Grounds a window's raw output. Ids are prefixed with `prefix` (`w3.e1`, `w3.m2`, `w3.a1`).
/// Ungrounded mentions/evidence are dropped; entities without grounded mentions and assertions
/// without grounded evidence (or referencing unknown entities) are dropped when
/// `drop_ungrounded` and counted.
pub fn ground_output(raw: RawOutput, labels: &HashMap<String, HashKey>, texts: &HashMap<HashKey, &str>, prefix: &str, provenance_idx: usize, drop_ungrounded: bool) -> Grounded {
    let mut g = Grounded::default();
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut mention_no = 0usize;
    let mut entity_no = 0usize;

    for e in raw.entities {
        let mut quotes = e.mentions;
        if let (Some(b), Some(q)) = (e.block, e.quote) {
            quotes.push(RawQuote { block: b, quote: q });
        }
        let name = if e.name.trim().is_empty() { quotes.first().map(|q| q.quote.trim().to_string()).unwrap_or_default() } else { e.name.trim().to_string() };
        if name.is_empty() {
            g.dropped_ungrounded += 1;
            continue;
        }
        let local_id = id_map.get(&e.id).cloned().unwrap_or_else(|| {
            entity_no += 1;
            format!("{prefix}.e{entity_no}")
        });
        let mut mentions = vec![];
        for q in &quotes {
            match ground_quote(&q.block, &q.quote, Some(&name), labels, texts) {
                Some((hk, s, en, surface)) => {
                    mention_no += 1;
                    let id = format!("{prefix}.m{mention_no}");
                    g.fragment.mentions.push(Mention { id: id.clone(), block_hk: hk, char_start: s, char_end: en, surface, raw_type: e.kind.trim().to_string(), confidence: 0.9 });
                    mentions.push(id);
                }
                None if drop_ungrounded => g.dropped_ungrounded += 1,
                None => {
                    let Some(hk) = labels.get(q.block.trim()) else { g.dropped_ungrounded += 1; continue };
                    mention_no += 1;
                    let id = format!("{prefix}.m{mention_no}");
                    g.fragment.mentions.push(Mention { id: id.clone(), block_hk: hk.clone(), char_start: 0, char_end: 0, surface: q.quote.trim().to_string(), raw_type: e.kind.trim().to_string(), confidence: 0.5 });
                    mentions.push(id);
                }
            }
        }
        if mentions.is_empty() && drop_ungrounded {
            g.dropped_ungrounded += 1;
            continue;
        }
        if let Some(existing) = g.fragment.entities.iter_mut().find(|x| x.local_id == local_id) {
            existing.mentions.extend(mentions);
            continue;
        }
        id_map.insert(e.id.clone(), local_id.clone());
        g.fragment.entities.push(DocEntity { local_id, name, raw_type: e.kind.trim().to_string(), type_id: None, aliases: vec![], mentions, entity_hk: None });
    }

    let mut assertion_no = 0usize;
    'outer: for a in raw.assertions {
        let predicate = a.predicate.trim().to_string();
        if predicate.is_empty() {
            g.dropped_ungrounded += 1;
            continue;
        }
        let mut args = vec![];
        for ra in a.args {
            let role = ra.role.trim().to_string();
            let value = if let Some(eid) = ra.entity.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                match id_map.get(eid) {
                    Some(local) => ArgValue::Entity { local_id: local.clone() },
                    None => {
                        g.dropped_ungrounded += 1;
                        continue 'outer;
                    }
                }
            } else {
                match ra.literal {
                    Some(RawLiteral::Obj { value, unit }) => ArgValue::Literal { value: literal_value(&value), unit: unit.filter(|u| !u.trim().is_empty()).or(ra.unit) },
                    Some(RawLiteral::Scalar(v)) => ArgValue::Literal { value: literal_value(&v), unit: ra.unit },
                    None => continue,
                }
            };
            args.push(Arg { role, value });
        }
        let mut evidence = vec![];
        for q in &a.evidence {
            match ground_quote(&q.block, &q.quote, None, labels, texts) {
                Some((hk, s, e, _)) => evidence.push(Evidence::Span { block_hk: hk, char_start: s, char_end: e }),
                None => g.dropped_ungrounded += drop_ungrounded as u32,
            }
        }
        if evidence.is_empty() && drop_ungrounded {
            g.dropped_ungrounded += 1;
            continue;
        }
        evidence.dedup();
        let kind = a.kind.unwrap_or_else(|| {
            let has = |r: &str| args.iter().any(|x| x.role == r);
            if has("subject") && has("object") {
                AssertionKind::Binary
            } else if has("subject") && has("value") {
                AssertionKind::Attribute
            } else {
                AssertionKind::Nary
            }
        });
        assertion_no += 1;
        g.fragment.assertions.push(Assertion {
            local_id: format!("{prefix}.a{assertion_no}"),
            predicate,
            predicate_id: None,
            kind,
            args,
            qualifiers: a.qualifiers,
            evidence,
            confidence: a.confidence.clamp(0.0, 1.0),
            verified: false,
            provenance_idx,
        });
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (HashMap<String, HashKey>, HashMap<HashKey, String>) {
        let b1 = HashKey::of(&["b1"]);
        let b2 = HashKey::of(&["b2"]);
        let labels = HashMap::from([("B1".to_string(), b1.clone()), ("B2".to_string(), b2.clone())]);
        let texts = HashMap::from([
            (b1, "We fine-tune BERT on the GLUE benchmark and reach 92.1 F1.".to_string()),
            (b2, "Our  model\noutperforms RoBERTa.".to_string()),
        ]);
        (labels, texts)
    }

    const RAW: &str = r#"{
      "entities": [
        {"id":"e1","name":"BERT","type":"method","mentions":[{"block":"B1","quote":"fine-tune BERT on"}]},
        {"id":"e2","name":"GLUE","type":"dataset","mentions":[{"block":"B1","quote":"the GLUE benchmark"},{"block":"B1","quote":"not in text"}]},
        {"id":"e3","name":"Ghost","type":"method","mentions":[{"block":"B2","quote":"hallucinated"}]},
        {"id":"e4","name":"RoBERTa","type":"method","mentions":[{"block":"B2","quote":"our model outperforms roberta"}]}
      ],
      "assertions": [
        {"predicate":"result","kind":"nary","args":[{"role":"method","entity":"e1","literal":null},{"role":"dataset","entity":"e2","literal":null},{"role":"value","entity":null,"literal":{"value":"92.1","unit":"F1"}}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B1","quote":"reach 92.1 F1"}],"confidence":0.9},
        {"predicate":"uses","kind":"binary","args":[{"role":"subject","entity":"e3","literal":null},{"role":"object","entity":"e2","literal":null}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B1","quote":"fine-tune"}],"confidence":0.9},
        {"predicate":"compares","kind":"binary","args":[{"role":"subject","entity":"e4","literal":null},{"role":"object","entity":"e1","literal":null}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B2","quote":"nowhere"}],"confidence":0.9},
        {"predicate":"compares","args":[{"role":"subject","entity":"e4"},{"role":"object","entity":"e1"}],"evidence":[{"block":"B9","quote":"x"},{"block":"B2","quote":"outperforms RoBERTa"}]}
      ]
    }"#;

    #[test]
    fn grounds_and_drops() {
        let (labels, texts) = setup();
        let texts_ref: HashMap<HashKey, &str> = texts.iter().map(|(k, v)| (k.clone(), v.as_str())).collect();
        let g = ground_output(parse_raw(RAW).unwrap(), &labels, &texts_ref, "w0", 4, true);
        let f = &g.fragment;
        // e3 dropped (no grounded mention); e2's second mention dropped
        assert_eq!(f.entities.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["BERT", "GLUE", "RoBERTa"]);
        assert_eq!(f.entities[0].local_id, "w0.e1");
        assert_eq!(f.mentions.len(), 3);
        // mention narrowed to the entity name inside the quote
        let m = &f.mentions[0];
        assert_eq!((m.surface.as_str(), m.char_start, m.char_end), ("BERT", 13, 17));
        // normalised match on B2 narrowed to "RoBERTa"
        assert_eq!(f.mentions[2].surface, "RoBERTa");
        // assertions: result kept; uses dropped (unknown entity e3); compares with bad quote dropped; last kept with one evidence, kind inferred
        assert_eq!(f.assertions.len(), 2);
        assert_eq!(f.assertions[0].predicate, "result");
        assert_eq!(f.assertions[0].args[2].value, ArgValue::Literal { value: "92.1".into(), unit: Some("F1".into()) });
        assert_eq!(f.assertions[0].args[0].value, ArgValue::Entity { local_id: "w0.e1".into() });
        assert_eq!(f.assertions[0].provenance_idx, 4);
        assert_eq!(f.assertions[1].kind, AssertionKind::Binary);
        assert_eq!(f.assertions[1].evidence.len(), 1);
        assert_eq!(f.assertions[1].local_id, "w0.a2");
        // drops: e2 mention, e3 mention, e3 entity, uses assertion, compares evidence + assertion, B9 evidence
        assert_eq!(g.dropped_ungrounded, 7);
    }

    #[test]
    fn keeps_ungrounded_when_configured() {
        let (labels, texts) = setup();
        let texts_ref: HashMap<HashKey, &str> = texts.iter().map(|(k, v)| (k.clone(), v.as_str())).collect();
        let g = ground_output(parse_raw(RAW).unwrap(), &labels, &texts_ref, "w0", 0, false);
        assert_eq!(g.fragment.entities.len(), 4);
        assert_eq!(g.fragment.assertions.len(), 4); // e3 survives with an unlocated mention, so nothing drops
        assert_eq!((g.fragment.mentions[3].char_start, g.fragment.mentions[3].char_end), (0, 0));
        assert!(g.fragment.assertions[2].evidence.is_empty());
    }

    #[test]
    fn parses_fenced_json() {
        let r = parse_raw("```json\n{\"entities\":[],\"assertions\":[]}\n```").unwrap();
        assert!(r.entities.is_empty());
        assert!(parse_raw("nope").is_err());
    }
}
