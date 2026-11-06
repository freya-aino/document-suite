use serde::{Deserialize, Serialize};

use vs_core::{Evidence, HashKey, Provenance};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mention {
    pub id: String,
    pub block_hk: HashKey,
    pub char_start: u32,
    pub char_end: u32,
    pub surface: String,
    pub raw_type: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocEntity {
    pub local_id: String,
    pub name: String,
    pub raw_type: String,
    /// Normalised ontology type id (after TypeNormalizer).
    pub type_id: Option<String>,
    pub aliases: Vec<String>,
    pub mentions: Vec<String>,
    /// Canonical entity after corpus resolution.
    pub entity_hk: Option<HashKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionKind {
    Binary,
    Nary,
    Attribute,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArgValue {
    Entity { local_id: String },
    Literal { value: String, unit: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Arg {
    pub role: String,
    pub value: ArgValue,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Qualifiers {
    #[serde(default)]
    pub negated: bool,
    #[serde(default)]
    pub hedged: bool,
    pub condition: Option<String>,
    pub time: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assertion {
    pub local_id: String,
    pub predicate: String,
    pub predicate_id: Option<String>,
    pub kind: AssertionKind,
    pub args: Vec<Arg>,
    pub qualifiers: Qualifiers,
    pub evidence: Vec<Evidence>,
    pub confidence: f32,
    pub verified: bool,
    /// Index into `ExtractionOutput::provenance`.
    pub provenance_idx: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionOutput {
    pub mentions: Vec<Mention>,
    pub entities: Vec<DocEntity>,
    pub assertions: Vec<Assertion>,
    pub provenance: Vec<Provenance>,
    pub dropped_ungrounded: u32,
    pub dropped_unverified: u32,
}
