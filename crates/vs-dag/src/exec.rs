use chrono::Utc;
use futures::future::join_all;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use vs_core::Provenance;

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
    pub outcome: NodeOutcome,
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
        let mut store: HashMap<String, Artifact> = HashMap::new();
        store.insert(SOURCE_PAGE_IMAGE.to_string(), page_image);
        store.insert(SOURCE_TEXT_LAYER.to_string(), text_layer);
        let mut traces = Vec::new();
        let dag_ref = dag.reference();

        for level in levels {
            let futs = level.iter().map(|&i| {
                let node = &dag.nodes[i];
                let step = self.registry.get(&node.step).expect("validated");
                let inputs: Vec<&Artifact> = node.inputs.iter().map(|k| &store[k]).collect();
                let mut nctx = ctx.clone();
                nctx.node_id = node.id.clone();
                nctx.dag_ref = dag_ref.clone();
                let timeout_ms = node.timeout_ms.unwrap_or_else(|| step.default_timeout_ms());
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
                let failure = match res {
                    Ok(Ok(out)) => {
                        prov.model_ref = out.model_ref;
                        prov.prompt_ref = out.prompt_ref;
                        store.insert(node.id.clone(), out.artifact);
                        traces.push(NodeTrace { node_id: node.id.clone(), provenance: prov, outcome: NodeOutcome::Ok });
                        continue;
                    }
                    Ok(Err(e)) => ExecError::Node { node_id: node.id.clone(), source: e },
                    Err(_) => ExecError::Timeout { node_id: node.id.clone(), timeout_ms },
                };
                match (node.optional, Artifact::empty(out_kind)) {
                    (true, Some(empty)) => {
                        tracing::warn!(node = %node.id, error = %failure, "optional node failed");
                        store.insert(node.id.clone(), empty);
                        traces.push(NodeTrace {
                            node_id: node.id.clone(),
                            provenance: prov,
                            outcome: NodeOutcome::SkippedAfterError(failure.to_string()),
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

/// First 16 hex chars of SHA-256 over the canonical JSON of params.
pub fn params_hash(params: &serde_json::Value) -> String {
    let s = serde_json::to_string(params).unwrap_or_default();
    hex::encode(&Sha256::digest(s.as_bytes())[..8])
}
