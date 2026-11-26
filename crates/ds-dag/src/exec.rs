use chrono::Utc;
use futures::future::join_all;
use tracing::Instrument;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use ds_core::Provenance;

use crate::artifact::{Artifact, BlockDraft};
use crate::dag::{Dag, DagError, SOURCE_PAGE_IMAGE, SOURCE_TEXT_LAYER};
use crate::step::{StepContext, StepError, StepRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeOutcome {
    Ok,
    /// Optional node failed; dependants got an empty artefact.
    SkippedAfterError(String),
}

#[derive(Debug, Clone)]
pub struct NodeTrace {
    pub node_id: String,
    pub provenance: Provenance,
    /// Zero-based executor level. Nodes of one level ran concurrently, which is the fact
    /// a critical path needs and which nothing downstream can recover from the timings.
    pub level: u32,
    pub outcome: NodeOutcome,
    /// What the step reported about how it ran: `StepOutput::metrics`. Empty for a step
    /// that reported nothing, and for a node that failed or was skipped.
    pub metrics: crate::step::Metrics,
}

#[derive(Debug)]
pub struct ExecOutput {
    pub blocks: Vec<BlockDraft>,
    pub traces: Vec<NodeTrace>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Invalid(#[from] DagError),
    #[error("node {node_id} failed: {source}")]
    Node { node_id: String, source: StepError },
    #[error("node {node_id} timed out after {timeout_ms} ms")]
    Timeout { node_id: String, timeout_ms: u64 },
    #[error("output node produced {0:?}, expected blocks")]
    BadOutput(crate::ArtifactKind),
}

pub struct Executor<'a> {
    pub registry: &'a StepRegistry,
}

impl<'a> Executor<'a> {
    pub fn new(registry: &'a StepRegistry) -> Self {
        Self { registry }
    }

    /// Runs one DAG for one page. `ctx.node_id` is overwritten per node.
    pub async fn run(
        &self,
        dag: &Dag,
        ctx: &StepContext,
        page_image: Artifact,
        text_layer: Artifact,
    ) -> Result<ExecOutput, ExecError> {
        let levels = dag.validate(self.registry)?;
        metrics::counter!("dag_pages_total", "dag" => dag.reference()).increment(1);
        let mut store: HashMap<String, Artifact> = HashMap::new();
        store.insert(SOURCE_PAGE_IMAGE.to_string(), page_image);
        store.insert(SOURCE_TEXT_LAYER.to_string(), text_layer);
        let mut traces = Vec::new();
        let dag_ref = dag.reference();

        for (level_no, level) in levels.into_iter().enumerate() {
            let level_no = level_no as u32;
            let futs = level.iter().map(|&i| {
                let node = &dag.nodes[i];
                let step = self.registry.get(&node.step).expect("validated");
                let inputs: Vec<&Artifact> = node.inputs.iter().map(|k| &store[k]).collect();
                let mut nctx = ctx.clone();
                nctx.node_id = node.id.clone();
                nctx.dag_ref = dag_ref.clone();
                let timeout_ms = node.timeout_ms.unwrap_or_else(|| step.default_timeout_ms());
                // One span per node, so a page's trace is the DAG's shape: `node_id` and
                // `page_no` are two of §5.5's standard fields and this is the only place
                // in the workspace that has them.
                let span = tracing::info_span!(
                    "dag_node",
                    node_id = %node.id,
                    step = %node.step,
                    dag = %dag_ref,
                    page_no = ctx.page_no,
                    run_id = %ctx.run_id,
                );
                async move {
                    let started_at = Utc::now();
                    let t0 = Instant::now();
                    let res = tokio::time::timeout(
                        Duration::from_millis(timeout_ms),
                        step.run(&nctx, &inputs, &node.params),
                    )
                    .await;
                    (i, res, started_at, t0.elapsed(), timeout_ms, step.signature().output)
                }
                .instrument(span)
            });

            for (i, res, started_at, elapsed, timeout_ms, out_kind) in join_all(futs).await {
                let node = &dag.nodes[i];
                let mut prov = Provenance {
                    run_id: ctx.run_id,
                    dag_ref: dag_ref.clone(),
                    node_id: node.id.clone(),
                    step_kind: node.step.clone(),
                    model_ref: None,
                    prompt_ref: None,
                    params_hash: params_hash(&node.params),
                    started_at,
                    duration_ms: elapsed.as_millis() as u64,
                };
                // Recorded for every node whatever the outcome, including the timeout: a
                // step that is slow enough to be killed is the one whose duration matters.
                metrics::histogram!("dag_node_duration_seconds", "step" => node.step.clone(), "dag" => dag_ref.clone())
                    .record(elapsed.as_secs_f64());
                let failure = match res {
                    Ok(Ok(out)) => {
                        prov.model_ref = out.model_ref;
                        prov.prompt_ref = out.prompt_ref;
                        store.insert(node.id.clone(), out.artifact);
                        traces.push(NodeTrace {
                            node_id: node.id.clone(),
                            provenance: prov,
                            level: level_no,
                            outcome: NodeOutcome::Ok,
                            metrics: out.metrics,
                        });
                        continue;
                    }
                    Ok(Err(e)) => {
                        failed(node, &dag_ref, step_error_reason(&e));
                        ExecError::Node { node_id: node.id.clone(), source: e }
                    }
                    Err(_) => {
                        failed(node, &dag_ref, "timeout");
                        ExecError::Timeout { node_id: node.id.clone(), timeout_ms }
                    }
                };
                match (node.optional, Artifact::empty(out_kind)) {
                    (true, Some(empty)) => {
                        tracing::warn!(node = %node.id, error = %failure, "optional node failed");
                        store.insert(node.id.clone(), empty);
                        traces.push(NodeTrace {
                            node_id: node.id.clone(),
                            provenance: prov,
                            level: level_no,
                            outcome: NodeOutcome::SkippedAfterError(failure.to_string()),
                            // A node that did not produce an artifact reported nothing
                            // about how it produced one.
                            metrics: Default::default(),
                        });
                    }
                    _ => return Err(failure),
                }
            }
        }

        match store.remove(&dag.output) {
            Some(Artifact::Blocks(blocks)) => Ok(ExecOutput { blocks, traces }),
            Some(other) => Err(ExecError::BadOutput(other.kind())),
            None => unreachable!("output validated"),
        }
    }
}

fn failed(node: &crate::dag::Node, dag_ref: &str, reason: &'static str) {
    metrics::counter!("dag_node_failures_total", "step" => node.step.clone(), "dag" => dag_ref.to_string(), "reason" => reason).increment(1);
}

/// A bounded label for `dag_node_failures_total{reason}`.
///
/// The error's own message is not usable here: it carries model names, parse offsets and
/// row counts, so one label per distinct failure would grow the series set without limit.
fn step_error_reason(e: &StepError) -> &'static str {
    match e {
        StepError::InvalidParams(_) => "invalid_params",
        StepError::Model(_) => "model",
        StepError::Parse(_) => "parse",
        StepError::Io(_) => "io",
        StepError::NotImplemented(_) => "not_implemented",
    }
}

/// First 16 hex chars of SHA-256 over the canonical JSON of params.
pub fn params_hash(params: &serde_json::Value) -> String {
    let s = serde_json::to_string(params).unwrap_or_default();
    hex::encode(&Sha256::digest(s.as_bytes())[..8])
}

impl From<&NodeTrace> for ds_core::NodeTrace {
    /// One conversion, so the stored page's trace and the executor's cannot disagree
    /// about what a node did.
    fn from(t: &NodeTrace) -> Self {
        ds_core::NodeTrace {
            node_id: t.node_id.clone(),
            step_kind: t.provenance.step_kind.clone(),
            model_ref: t.provenance.model_ref.clone(),
            prompt_ref: t.provenance.prompt_ref.clone(),
            duration_ms: t.provenance.duration_ms,
            level: t.level,
            outcome: match &t.outcome {
                NodeOutcome::Ok => "ok".into(),
                NodeOutcome::SkippedAfterError(_) => "skipped_after_error".into(),
            },
            metrics: t.metrics.clone(),
        }
    }
}
