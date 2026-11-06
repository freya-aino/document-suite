//! Batched assertion verification against evidence text.

use std::collections::HashMap;

use serde::Deserialize;
use vs_core::{Evidence, HashKey};

use crate::model::{ArgValue, Assertion, DocEntity};
use crate::ExtractError;

/// Human-readable form of an assertion: `result(method=BERT; value=92.1 F1) [hedged]`.
pub fn describe_assertion(a: &Assertion, entities: &[DocEntity]) -> String {
    let name = |id: &str| entities.iter().find(|e| e.local_id == id).map(|e| e.name.clone()).unwrap_or_else(|| id.to_string());
    let args: Vec<String> = a
        .args
        .iter()
        .map(|arg| match &arg.value {
            ArgValue::Entity { local_id } => format!("{}={}", arg.role, name(local_id)),
            ArgValue::Literal { value, unit } => format!("{}={}{}", arg.role, value, unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default()),
        })
        .collect();
    let mut s = format!("{}({})", a.predicate, args.join("; "));
    let mut quals = vec![];
    if a.qualifiers.negated {
        quals.push("negated".to_string());
    }
    if a.qualifiers.hedged {
        quals.push("hedged".to_string());
    }
    if let Some(c) = &a.qualifiers.condition {
        quals.push(format!("condition: {c}"));
    }
    if let Some(t) = &a.qualifiers.time {
        quals.push(format!("time: {t}"));
    }
    if !quals.is_empty() {
        s.push_str(&format!(" [{}]", quals.join(", ")));
    }
    s
}

/// Evidence quotes of an assertion joined with ` | `; `None` when no span evidence exists.
pub fn evidence_text(a: &Assertion, texts: &HashMap<HashKey, &str>) -> Option<String> {
    let quotes: Vec<String> = a
        .evidence
        .iter()
        .filter_map(|ev| match ev {
            Evidence::Span { block_hk, char_start, char_end } => {
                let t = texts.get(block_hk)?;
                Some(t.chars().skip(*char_start as usize).take((*char_end - *char_start) as usize).collect::<String>())
            }
            Evidence::Region { .. } => None,
        })
        .filter(|q| !q.trim().is_empty())
        .collect();
    (!quotes.is_empty()).then(|| quotes.join(" | "))
}

/// Renders the `{items}` section of the verify prompt for the given assertions.
pub fn render_items(assertions: &[&Assertion], entities: &[DocEntity], texts: &HashMap<HashKey, &str>) -> String {
    let mut out = String::new();
    for a in assertions {
        let ev = evidence_text(a, texts).unwrap_or_default();
        out.push_str(&format!("- id: {}\n  assertion: {}\n  evidence: \"{}\"\n", a.local_id, describe_assertion(a, entities), ev.replace('"', "'")));
    }
    out
}

#[derive(Debug, Deserialize)]
struct Verdicts {
    #[serde(default)]
    verdicts: Vec<Verdict>,
}

#[derive(Debug, Deserialize)]
struct Verdict {
    id: String,
    supported: bool,
}

/// Parses the verify model's JSON into `assertion local_id → supported`.
pub fn parse_verdicts(text: &str) -> Result<HashMap<String, bool>, ExtractError> {
    let t = text.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let v: Verdicts = serde_json::from_str(t).map_err(|e| ExtractError::Schema(format!("verify output: {e}")))?;
    Ok(v.verdicts.into_iter().map(|v| (v.id.trim().to_string(), v.supported)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arg, AssertionKind, Qualifiers};

    #[test]
    fn describes_and_renders() {
        let hk = HashKey::of(&["b"]);
        let e = DocEntity { local_id: "e1".into(), name: "BERT".into(), raw_type: "method".into(), type_id: None, aliases: vec![], mentions: vec![], entity_hk: None };
        let a = Assertion {
            local_id: "a1".into(),
            predicate: "result".into(),
            predicate_id: None,
            kind: AssertionKind::Nary,
            args: vec![
                Arg { role: "method".into(), value: ArgValue::Entity { local_id: "e1".into() } },
                Arg { role: "value".into(), value: ArgValue::Literal { value: "92.1".into(), unit: Some("F1".into()) } },
            ],
            qualifiers: Qualifiers { hedged: true, ..Default::default() },
            evidence: vec![Evidence::Span { block_hk: hk.clone(), char_start: 4, char_end: 12 }],
            confidence: 0.9,
            verified: false,
            provenance_idx: 0,
        };
        let entities = [e];
        assert_eq!(describe_assertion(&a, &entities), "result(method=BERT; value=92.1 F1) [hedged]");
        let texts = HashMap::from([(hk, "The \"quoted\" part.")]);
        let r = render_items(&[&a], &entities, &texts);
        assert_eq!(r, "- id: a1\n  assertion: result(method=BERT; value=92.1 F1) [hedged]\n  evidence: \"'quoted'\"\n");
        let v = parse_verdicts(r#"{"verdicts":[{"id":"a1","supported":true},{"id":"a2","supported":false}]}"#).unwrap();
        assert!(v["a1"]);
        assert!(!v["a2"]);
    }
}
