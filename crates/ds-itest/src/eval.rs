//! Turning what the API served into the scorer's own `PageEval`.
//!
//! This is the piece the whole black-box suite turns on. `ds-adapt`'s scorer consumes a
//! `RunRecord` plus an in-process result; a deployed system hands back a stored
//! `ds_core::PageResult` over HTTP. Rather than write a second scoring path — which would
//! be free to drift and would make a compose run and a `ds-adapt` run disagree about the
//! same document — the result is mapped with `score::result_blocks` and handed to
//! `score::score_blocks`, the same arithmetic on the same fields.
//!
//! The output is a `PageEval`, written with `records::write_jsonl`, so `evals.jsonl` from
//! here is **byte-identical in shape** to `ds-adapt score`'s. `ds-adapt select` and
//! `report::render` then run on black-box data unchanged, which is the point of routing
//! through `PageEval` at all.
//!
//! The gap this used to carry is closed. `parse_paths` and the host/model latency split
//! both come from node traces, and a page read back over the API used to have none — a
//! block records the node that produced it, which cannot account for a node that produced
//! no block, and the executor's levels were not recoverable at all. `PageResult` now
//! carries `traces` (schema version 2), so a compose run reports its parse paths and its
//! critical path exactly as an in-process run does. A stored page from before the bump
//! has no traces, and the split is then `None` rather than zero.

use ds_adapt::metrics::EvalBlock;
use ds_adapt::records::RunRecord;
use ds_adapt::score::{self, MetricWeights};
use ds_adapt::select::PageEval;

use crate::client::JobStatus;

/// What one uploaded page cost, as the deployed system reports it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cost {
    /// Wall-clock milliseconds the document took, server-side, from the job's own state
    /// ladder. Per **document**, not per page: there is no end-to-end latency metric, and
    /// the stage histograms are fleet aggregates.
    pub document_ms: f32,
    /// Seconds of model-node wall time, from the blocks' own provenance.
    ///
    /// Named for what it is. It includes the network and whatever queueing happened at
    /// the endpoint, and on a CPU tier it involves no GPU at all — so it is a cost proxy
    /// and never a GPU-utilisation figure.
    pub model_seconds: f32,
    /// Prompt plus completion tokens the document cost, from the `model_tokens_total`
    /// delta around it.
    ///
    /// This is the whole reason the harness scrapes rather than only times.
    /// `RunRecord::tokens` is hard-wired to 0 on every in-process path because no token
    /// count reaches the executor — `ds_dag::Provenance` has no token fields — but the
    /// model client increments the counter, so a delta over one document gives the number
    /// without any production change.
    pub tokens: u32,
}

/// Builds the eval for one page of one document.
///
/// `dag_ref` and the page profile come from what the service actually did, so the class
/// key a report groups by is the deployment's own routing decision rather than a
/// re-derivation of it here.
pub fn page_eval(
    doc_id: &str,
    page: &ds_core::PageResult,
    gt: &[EvalBlock],
    cost: Cost,
    weights: &MetricWeights,
) -> PageEval {
    let (dag_id, dag_version) = split_ref(&page.dag_ref);
    // From the executor's own levels, carried in the stored page. `None` on a version 1
    // page, and left at zero then — which the report prints as "not measured" rather
    // than as instant.
    let (host_ms, model_ms) = page.critical_path_ms().unwrap_or((0, 0));
    let rec = RunRecord {
        doc: doc_id.to_string(),
        page: page.page_no,
        dag_id: dag_id.to_string(),
        dag_version: dag_version.to_string(),
        class_key: ds_adapt::select::key_string(&ds_adapt::select::class_key(&page.profile, 0)),
        profile_json: page.profile.clone(),
        latency_ms: cost.document_ms,
        gpu_seconds: cost.model_seconds,
        host_ms,
        model_ms,
        tokens: cost.tokens,
        ok: true,
        error: None,
        result_path: None,
        image_path: None,
        ground_truth_uri: None,
        split: None,
        blocks: page.blocks.len() as u32,
    };
    let pred = score::result_blocks(&page.blocks);
    let mut eval = score::score_blocks(&rec, Some(&pred), gt, weights);
    // The parse paths, from the same traces. A model whose family is `auto` reports which
    // path won, and that is the one fact that says whether a newly registered model is
    // being read structurally or split as Markdown — the question `auto` exists for.
    eval.parse_paths = score::parse_paths(&page.traces);
    eval
}

/// Model-node wall time for one page, summed from the blocks' provenance.
///
/// Summed over the **distinct nodes** that reported a model, not over blocks: one
/// `page.parse_e2e` call produces many blocks, each carrying the same node's duration, so
/// summing per block multiplies one model call by the number of blocks it returned.
pub fn model_seconds(page: &ds_core::PageResult) -> f32 {
    let mut seen: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for b in &page.blocks {
        if b.provenance.model_ref.is_some() {
            seen.insert(&b.provenance.node_id, b.provenance.duration_ms);
        }
    }
    seen.values().map(|ms| *ms as f32 / 1000.0).sum()
}

/// Document wall time from the job's state ladder, `received` to its terminal state.
pub fn document_ms(job: &JobStatus) -> f32 {
    job.between("received", "done")
        .or_else(|| job.between("received", "partial"))
        .unwrap_or(0) as f32
}

fn split_ref(r: &str) -> (&str, &str) {
    r.split_once('@').unwrap_or((r, "unknown"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dag_reference_splits_into_id_and_version() {
        assert_eq!(split_ref("vlm-e2e-robust@1"), ("vlm-e2e-robust", "1"));
        assert_eq!(split_ref("odd"), ("odd", "unknown"));
    }
}
