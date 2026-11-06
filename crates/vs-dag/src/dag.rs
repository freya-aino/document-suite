use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::artifact::ArtifactKind;

pub const SOURCE_PAGE_IMAGE: &str = "$page_image";
pub const SOURCE_TEXT_LAYER: &str = "$text_layer";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    /// Step kind, e.g. `table.recognize`.
    pub step: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub params: serde_json::Value,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dag {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    /// Node whose artefact (kind `blocks`) is the page result.
    pub output: String,
    pub nodes: Vec<Node>,
}

/// Input kinds (ordered) and output kind of a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub inputs: Vec<ArtifactKind>,
    pub output: ArtifactKind,
}

/// Lookup of step signatures; implemented by `StepRegistry` (and by test fakes).
pub trait StepSignatures {
    fn signature(&self, step_kind: &str) -> Option<Signature>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DagError {
    #[error("duplicate node id {0:?}")]
    DuplicateNode(String),
    #[error("node id {0:?} must not start with '$'")]
    ReservedId(String),
    #[error("node {node:?} references unknown input {input:?}")]
    UnknownInput { node: String, input: String },
    #[error("cycle detected involving nodes {0:?}")]
    Cycle(Vec<String>),
    #[error("output node {0:?} does not exist")]
    MissingOutput(String),
    #[error("output node {0:?} must produce blocks")]
    OutputNotBlocks(String),
    #[error("node {node:?} uses unknown step {step:?}")]
    UnknownStep { node: String, step: String },
    #[error("node {node:?}: expected inputs {expected:?}, got {got:?}")]
    InputMismatch { node: String, expected: Vec<ArtifactKind>, got: Vec<ArtifactKind> },
}

impl Dag {
    /// `id@version`, used in provenance and routing.
    pub fn reference(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    /// Validates the DAG (see SPEC.md) and returns execution levels as node indices.
    /// Nodes not on a path to `output` are pruned and do not appear in the levels.
    pub fn validate(&self, steps: &dyn StepSignatures) -> Result<Vec<Vec<usize>>, DagError> {
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if n.id.starts_with('$') {
                return Err(DagError::ReservedId(n.id.clone()));
            }
            if index.insert(n.id.as_str(), i).is_some() {
                return Err(DagError::DuplicateNode(n.id.clone()));
            }
        }

        let source_kind = |s: &str| match s {
            SOURCE_PAGE_IMAGE => Some(ArtifactKind::PageImage),
            SOURCE_TEXT_LAYER => Some(ArtifactKind::TextLayer),
            _ => None,
        };

        // Signatures and input references.
        let mut sigs: Vec<Signature> = Vec::with_capacity(self.nodes.len());
        for n in &self.nodes {
            let sig = steps
                .signature(&n.step)
                .ok_or_else(|| DagError::UnknownStep { node: n.id.clone(), step: n.step.clone() })?;
            for inp in &n.inputs {
                if source_kind(inp).is_none() && !index.contains_key(inp.as_str()) {
                    return Err(DagError::UnknownInput { node: n.id.clone(), input: inp.clone() });
                }
            }
            sigs.push(sig);
        }

        // Input kind check.
        for (i, n) in self.nodes.iter().enumerate() {
            let got: Vec<ArtifactKind> = n
                .inputs
                .iter()
                .map(|inp| source_kind(inp).unwrap_or_else(|| sigs[index[inp.as_str()]].output))
                .collect();
            if got != sigs[i].inputs {
                return Err(DagError::InputMismatch { node: n.id.clone(), expected: sigs[i].inputs.clone(), got });
            }
        }

        let out = *index.get(self.output.as_str()).ok_or_else(|| DagError::MissingOutput(self.output.clone()))?;
        if sigs[out].output != ArtifactKind::Blocks {
            return Err(DagError::OutputNotBlocks(self.output.clone()));
        }

        // Prune: keep nodes reachable backwards from output.
        let mut needed: HashSet<usize> = HashSet::new();
        let mut stack = vec![out];
        while let Some(i) = stack.pop() {
            if needed.insert(i) {
                for inp in &self.nodes[i].inputs {
                    if let Some(&j) = index.get(inp.as_str()) {
                        stack.push(j);
                    }
                }
            }
        }

        // Kahn's algorithm by levels over needed nodes.
        let mut remaining: HashSet<usize> = needed.clone();
        let mut done: HashSet<usize> = HashSet::new();
        let mut levels = Vec::new();
        while !remaining.is_empty() {
            let mut level: Vec<usize> = remaining
                .iter()
                .copied()
                .filter(|&i| {
                    self.nodes[i]
                        .inputs
                        .iter()
                        .all(|inp| source_kind(inp).is_some() || done.contains(&index[inp.as_str()]))
                })
                .collect();
            if level.is_empty() {
                let mut ids: Vec<String> = remaining.iter().map(|&i| self.nodes[i].id.clone()).collect();
                ids.sort();
                return Err(DagError::Cycle(ids));
            }
            level.sort_unstable();
            for &i in &level {
                remaining.remove(&i);
                done.insert(i);
            }
            levels.push(level);
        }
        Ok(levels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ArtifactKind::*;

    struct Fake(HashMap<&'static str, Signature>);
    impl StepSignatures for Fake {
        fn signature(&self, k: &str) -> Option<Signature> {
            self.0.get(k).cloned()
        }
    }

    fn steps() -> Fake {
        let mut m = HashMap::new();
        m.insert("layout.detect", Signature { inputs: vec![PageImage], output: Layout });
        m.insert("table.recognize", Signature { inputs: vec![PageImage, Layout], output: Blocks });
        m.insert("blocks.merge_native", Signature { inputs: vec![TextLayer, Layout, Blocks], output: Blocks });
        m.insert("page.parse_e2e", Signature { inputs: vec![PageImage], output: Blocks });
        Fake(m)
    }

    fn node(id: &str, step: &str, inputs: &[&str]) -> Node {
        Node {
            id: id.into(),
            step: step.into(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            params: serde_json::Value::Null,
            timeout_ms: None,
            optional: false,
        }
    }

    fn dag(nodes: Vec<Node>, output: &str) -> Dag {
        Dag { id: "t".into(), version: "1".into(), description: None, output: output.into(), nodes }
    }

    #[test]
    fn valid_dag_levels() {
        let d = dag(
            vec![
                node("layout", "layout.detect", &["$page_image"]),
                node("tables", "table.recognize", &["$page_image", "layout"]),
                node("merge", "blocks.merge_native", &["$text_layer", "layout", "tables"]),
                node("unused", "page.parse_e2e", &["$page_image"]),
            ],
            "merge",
        );
        let levels = d.validate(&steps()).unwrap();
        assert_eq!(levels, vec![vec![0], vec![1], vec![2]]); // "unused" pruned
    }

    #[test]
    fn detects_cycle() {
        let mut m = steps();
        m.0.insert("x", Signature { inputs: vec![Blocks], output: Blocks });
        let d = dag(vec![node("a", "x", &["b"]), node("b", "x", &["a"])], "a");
        assert!(matches!(d.validate(&m), Err(DagError::Cycle(_))));
    }

    #[test]
    fn detects_kind_mismatch() {
        let d = dag(vec![node("t", "table.recognize", &["$page_image", "$text_layer"])], "t");
        assert!(matches!(d.validate(&steps()), Err(DagError::InputMismatch { .. })));
    }

    #[test]
    fn output_must_be_blocks() {
        let d = dag(vec![node("layout", "layout.detect", &["$page_image"])], "layout");
        assert_eq!(d.validate(&steps()), Err(DagError::OutputNotBlocks("layout".into())));
    }

    #[test]
    fn unknown_input_and_step() {
        let d = dag(vec![node("a", "page.parse_e2e", &["nope"])], "a");
        assert!(matches!(d.validate(&steps()), Err(DagError::UnknownInput { .. })));
        let d = dag(vec![node("a", "nope.step", &[])], "a");
        assert!(matches!(d.validate(&steps()), Err(DagError::UnknownStep { .. })));
    }

    #[test]
    fn parses_json() {
        let j = r#"{"id":"e2e","version":"1","description":null,"output":"p",
                    "nodes":[{"id":"p","step":"page.parse_e2e","inputs":["$page_image"],
                              "params":{"model":"paddleocr-vl-1.5"}}]}"#;
        let d: Dag = serde_json::from_str(j).unwrap();
        assert_eq!(d.reference(), "e2e@1");
        d.validate(&steps()).unwrap();
    }
}
