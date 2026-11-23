use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{BBox, HashKey, RunId};

/// Who/what produced an output. Required on every block, mention and assertion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub run_id: RunId,
    /// `dag_id@version`, or `stage2@version` for document-level outputs.
    pub dag_ref: String,
    pub node_id: String,
    pub step_kind: String,
    /// `model_id@revision`; `None` for pure CPU steps.
    pub model_ref: Option<String>,
    /// `prompt_id@sha256-prefix`; `None` if no prompt was used.
    pub prompt_ref: Option<String>,
    pub params_hash: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
}

/// Where in the source a fact comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Evidence {
    /// Character span in `Block::text`.
    Span { block_hk: HashKey, char_start: u32, char_end: u32 },
    /// Visual region (figure parts, stamps, handwriting without reliable text).
    Region { page_hk: HashKey, bbox: BBox },
    /// One cell of a table block, by row and column.
    ///
    /// A table reaches extraction as HTML and its plain-text projection is the cells run
    /// together, so a character span into that projection is the wrong instrument for a
    /// table: `92.1` appearing twice in a results table gives two identical spans and the
    /// citation points at whichever came first. The row and column are exactly what makes
    /// a value citable — they are also what `vault.hub_cell` is keyed by and what the
    /// graph's `Cell` nodes carry, so this evidence resolves to a node a reader can fetch.
    Cell { block_hk: HashKey, row: u32, col: u32 },
}
