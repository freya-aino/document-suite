//! On-disk records shared by `run` and `score`.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::Path;

use ds_core::PageProfile;
use ds_dag::BlockDraft;

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
    /// Critical path through the DAG, split: milliseconds spent in **our** code and
    /// milliseconds spent waiting on the model tier.
    ///
    /// `host_ms` is the gateable one — it is what a code change moves. `model_ms` is
    /// reported and comparable only against the same host's model probe, because a
    /// faster GPU and a shorter prompt look identical in it.
    ///
    /// `latency_ms - (host_ms + model_ms)` is the executor's own residual, and it is
    /// meant to be small; a large one at `--parallel 1` means the measurement is wrong
    /// rather than the pipeline slow. Zero on a record from before the split existed,
    /// which is why both are `default`.
    #[serde(default)]
    pub host_ms: u64,
    #[serde(default)]
    pub model_ms: u64,
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

/// One node of the DAG, as it ran.
///
/// `ds_core::NodeTrace` rather than a type of its own, which it used to be: the stored
/// `PageResult` now carries the same traces, so an offline run and a run read back over
/// the API produce the *same* records and the scorer cannot be given two different
/// accounts of one page. The outcome strings have to agree for that to hold, which is why
/// the conversion lives in `ds-dag` and not here.
pub type TraceSummary = ds_core::NodeTrace;

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
