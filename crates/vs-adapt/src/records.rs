//! On-disk records shared by `run` and `score`.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::Path;

use vs_core::PageProfile;
use vs_dag::BlockDraft;

/// One line of `runs.jsonl`: one DAG on one page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub doc: String,
    pub page: u32,
    pub dag_id: String,
    pub dag_version: String,
    /// Discretised profile (`select::key_string(class_key(profile, 0))`).
    pub class_key: String,
    pub profile_json: PageProfile,
    pub latency_ms: f32,
    /// Estimated from model-node wall time (no token accounting in the executor yet).
    pub gpu_seconds: f32,
    pub tokens: u32,
    pub ok: bool,
    pub error: Option<String>,
    /// `out/results/<doc>/<page>/<dag>.json` (ok runs only).
    #[serde(default)]
    pub result_path: Option<String>,
    /// `out/pages/<doc>/<page>.png` (for the judge).
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub ground_truth_uri: Option<String>,
    #[serde(default)]
    pub split: Option<String>,
    #[serde(default)]
    pub blocks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    pub node_id: String,
    pub step_kind: String,
    pub model_ref: Option<String>,
    pub prompt_ref: Option<String>,
    pub duration_ms: u64,
    pub outcome: String,
}

/// `PageResult`-like output of one DAG on one page (block drafts, no hash keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptPageResult {
    pub doc: String,
    pub page_no: u32,
    pub dag_ref: String,
    pub profile: PageProfile,
    pub render_width_px: u32,
    pub render_height_px: u32,
    pub blocks: Vec<BlockDraft>,
    pub traces: Vec<TraceSummary>,
}

pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| anyhow::anyhow!("{}:{}: {e}", path.display(), i + 1)))
        .collect()
}

pub fn write_jsonl<T: Serialize>(path: &Path, items: &[T]) -> anyhow::Result<()> {
    let mut out = String::new();
    for it in items {
        out.push_str(&serde_json::to_string(it)?);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// File-system safe name for a DAG id (`e2e/model=a` → `e2e__model=a`).
pub fn safe_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | '\\' => out.push_str("__"),
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_whitespace() => out.push('_'),
            c => out.push(c),
        }
    }
    out
}
