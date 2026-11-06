//! Candidate generation: templates × slot values → validated DAGs (SPEC.md "Candidate generation").

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use vs_dag::{Dag, Node, StepSignatures};

/// `search_space.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSpace {
    pub templates: Vec<Template>,
    /// Cap over all templates (in file order); `None` = unlimited.
    #[serde(default)]
    pub max_candidates: Option<usize>,
}

/// A DAG skeleton whose JSON contains `{slot}` placeholders in string values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Skeleton: `output` and `nodes` (and optionally `description`); `id`/`version` are generated.
    pub dag: Value,
    /// Slot name → allowed values. Values are JSON so a slot can also carry numbers (e.g. DPI).
    #[serde(default)]
    pub slots: BTreeMap<String, Vec<Value>>,
}

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("template {template:?}: slot {slot:?} has no values")]
    EmptySlot { template: String, slot: String },
    #[error("template {template:?}: skeleton is not a DAG: {reason}")]
    BadSkeleton { template: String, reason: String },
}

/// One rejected candidate (kept for the report).
#[derive(Debug, Clone, Serialize)]
pub struct Rejected {
    pub id: String,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct Generated {
    pub candidates: Vec<Dag>,
    pub rejected: Vec<Rejected>,
    /// Candidates not generated because of `max_candidates`.
    pub truncated: usize,
}

/// Cartesian product of the slot values of one template, in slot-name order
/// (BTreeMap), first slot varying slowest.
pub fn assignments(slots: &BTreeMap<String, Vec<Value>>) -> Vec<Vec<(&str, &Value)>> {
    let mut out: Vec<Vec<(&str, &Value)>> = vec![vec![]];
    for (name, values) in slots {
        let mut next = Vec::with_capacity(out.len() * values.len());
        for prefix in &out {
            for v in values {
                let mut a = prefix.clone();
                a.push((name.as_str(), v));
                next.push(a);
            }
        }
        out = next;
    }
    out
}

/// Replaces `{slot}` in every string of the JSON tree. A string that is exactly one
/// placeholder takes the slot value itself (so numbers stay numbers); otherwise the
/// value's string form is substituted.
pub fn substitute(v: &Value, assignment: &[(&str, &Value)]) -> Value {
    match v {
        Value::String(s) => {
            for (name, val) in assignment {
                if s == &format!("{{{name}}}") {
                    return (*val).clone();
                }
            }
            let mut out = s.clone();
            for (name, val) in assignment {
                let text = match val {
                    Value::String(t) => t.clone(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{{name}}}"), &text);
            }
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| substitute(i, assignment)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, val)| (k.clone(), substitute(val, assignment))).collect()),
        other => other.clone(),
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `<template>/<slot=value,...>`; `<template>` alone when the template has no slots.
pub fn candidate_id(template: &str, assignment: &[(&str, &Value)]) -> String {
    if assignment.is_empty() {
        return template.to_string();
    }
    let slots: Vec<String> = assignment.iter().map(|(k, v)| format!("{k}={}", value_text(v))).collect();
    format!("{template}/{}", slots.join(","))
}

/// First 16 hex chars of SHA-256 over the canonical JSON (`serde_json` with sorted keys)
/// of `output` + `nodes`.
pub fn content_version(output: &str, nodes: &[Node]) -> String {
    let canonical = serde_json::json!({ "output": output, "nodes": nodes });
    let canon = canonicalize(&canonical);
    hex::encode(&Sha256::digest(serde_json::to_string(&canon).unwrap_or_default().as_bytes())[..8])
}

/// Recursively sorts object keys (serde_json::Map keeps insertion order by default).
fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<&String, Value> = m.iter().map(|(k, v)| (k, canonicalize(v))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// Expands every template and validates each candidate against `steps`.
pub fn generate(space: &SearchSpace, steps: &dyn StepSignatures) -> Result<Generated, GenerateError> {
    let mut out = Generated::default();
    let mut produced = 0usize;
    for t in &space.templates {
        for (slot, values) in &t.slots {
            if values.is_empty() {
                return Err(GenerateError::EmptySlot { template: t.id.clone(), slot: slot.clone() });
            }
        }
        for assignment in assignments(&t.slots) {
            if space.max_candidates.is_some_and(|max| produced >= max) {
                out.truncated += 1;
                continue;
            }
            produced += 1;
            let id = candidate_id(&t.id, &assignment);
            let filled = substitute(&t.dag, &assignment);
            let skeleton: Skeleton = serde_json::from_value(filled)
                .map_err(|e| GenerateError::BadSkeleton { template: t.id.clone(), reason: e.to_string() })?;
            let version = content_version(&skeleton.output, &skeleton.nodes);
            let dag = Dag {
                id: id.clone(),
                version,
                description: skeleton.description.or_else(|| t.description.clone()),
                output: skeleton.output,
                nodes: skeleton.nodes,
            };
            match dag.validate(steps) {
                Ok(_) => out.candidates.push(dag),
                Err(e) => out.rejected.push(Rejected { id, error: e.to_string() }),
            }
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct Skeleton {
    output: String,
    nodes: Vec<Node>,
    #[serde(default)]
    description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use vs_dag::{ArtifactKind, Signature};

    struct Fake(HashMap<&'static str, Signature>);
    impl StepSignatures for Fake {
        fn signature(&self, k: &str) -> Option<Signature> {
            self.0.get(k).cloned()
        }
    }

    fn steps() -> Fake {
        use ArtifactKind::*;
        let mut m = HashMap::new();
        m.insert("page.parse_e2e", Signature { inputs: vec![PageImage], output: Blocks });
        m.insert("layout.detect", Signature { inputs: vec![PageImage], output: Layout });
        m.insert("table.recognize", Signature { inputs: vec![PageImage, Layout], output: Blocks });
        m.insert("blocks.merge_native", Signature { inputs: vec![TextLayer, Layout, Blocks], output: Blocks });
        Fake(m)
    }

    fn space() -> SearchSpace {
        serde_json::from_value(json!({
            "max_candidates": 10,
            "templates": [
                { "id": "e2e", "slots": { "model": ["a", "b"], "dpi": [150, 200] },
                  "dag": { "output": "p", "nodes": [
                    { "id": "p", "step": "page.parse_e2e", "inputs": ["$page_image"],
                      "params": { "model": "{model}", "dpi": "{dpi}", "label": "m={model}" } } ] } },
                { "id": "bad", "slots": {},
                  "dag": { "output": "p", "nodes": [
                    { "id": "p", "step": "nope", "inputs": ["$page_image"] } ] } }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn cartesian_product_and_ids() {
        let g = generate(&space(), &steps()).unwrap();
        assert_eq!(g.candidates.len(), 4);
        assert_eq!(g.rejected.len(), 1);
        assert!(g.rejected[0].error.contains("unknown step"));
        let ids: Vec<&str> = g.candidates.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["e2e/dpi=150,model=a", "e2e/dpi=150,model=b", "e2e/dpi=200,model=a", "e2e/dpi=200,model=b"]);
    }

    #[test]
    fn substitution_keeps_value_types_and_replaces_inside_strings() {
        let g = generate(&space(), &steps()).unwrap();
        let p = &g.candidates[0].nodes[0].params;
        assert_eq!(p["model"], json!("a"));
        assert_eq!(p["dpi"], json!(150)); // whole-string placeholder takes the JSON value
        assert_eq!(p["label"], json!("m=a"));
    }

    #[test]
    fn version_is_content_hash() {
        let g = generate(&space(), &steps()).unwrap();
        assert_eq!(g.candidates[0].version.len(), 16);
        assert_ne!(g.candidates[0].version, g.candidates[1].version);
        // same content → same version regardless of id
        let v1 = content_version(&g.candidates[0].output, &g.candidates[0].nodes);
        assert_eq!(v1, g.candidates[0].version);
        // key order does not matter
        let a = canonicalize(&json!({"b": 1, "a": {"d": 2, "c": 3}}));
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn max_candidates_truncates() {
        let mut s = space();
        s.max_candidates = Some(3);
        let g = generate(&s, &steps()).unwrap();
        assert_eq!(g.candidates.len() + g.rejected.len(), 3);
        assert_eq!(g.truncated, 2);
    }

    #[test]
    fn empty_slot_is_an_error() {
        let mut s = space();
        s.templates[0].slots.insert("x".into(), vec![]);
        assert!(matches!(generate(&s, &steps()), Err(GenerateError::EmptySlot { .. })));
    }
}
