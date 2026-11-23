use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use vs_core::{HashKey, PageProfile, RunId};

use crate::artifact::Artifact;
use crate::dag::{Signature, StepSignatures};

/// Read-only context passed to every step.
#[derive(Debug, Clone)]
pub struct StepContext {
    pub run_id: RunId,
    pub document_hk: HashKey,
    pub page_hk: HashKey,
    pub page_no: u32,
    pub profile: PageProfile,
    pub dag_ref: String,
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct StepOutput {
    pub artifact: Artifact,
    /// `model_id@revision` if a model was called.
    pub model_ref: Option<String>,
    /// `prompt_id@hash` if a prompt was used.
    pub prompt_ref: Option<String>,
    /// Anything a step learned about *how* it produced this, for the node's trace.
    ///
    /// Flat `{name: value}`, small, and never the output itself — this travels into the
    /// `NodeTrace` and from there into the vault's `sat_node_metrics`, so a step that put
    /// its blocks in here would store them twice.
    ///
    /// It exists because of one concrete gap: the `auto` output family tries several
    /// parsers and the one that wins is the single most useful fact about the call, and
    /// there was nowhere to put it. It was logged to a `tracing` target instead, which an
    /// operator could read and `vs-adapt` could not, so "which parse path did this model
    /// take" was unscoreable — and that is the question `auto` exists to answer.
    pub metrics: Metrics,
}

/// Per-node metadata: a handful of named scalars.
pub type Metrics = std::collections::BTreeMap<String, serde_json::Value>;

impl StepOutput {
    /// An artifact with no model and no metadata behind it: the CPU steps.
    pub fn plain(artifact: Artifact) -> Self {
        Self { artifact, model_ref: None, prompt_ref: None, metrics: Metrics::new() }
    }

    /// An artifact a model produced.
    pub fn from_model(artifact: Artifact, model_ref: String, prompt_ref: String) -> Self {
        Self { artifact, model_ref: Some(model_ref), prompt_ref: Some(prompt_ref), metrics: Metrics::new() }
    }

    /// Records one named fact about how this output was produced.
    pub fn with_metric(mut self, name: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.metrics.insert(name.into(), value.into());
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StepError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("model call failed: {0}")]
    Model(String),
    #[error("could not parse model output: {0}")]
    Parse(String),
    #[error("i/o: {0}")]
    Io(String),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

#[async_trait]
pub trait Step: Send + Sync {
    fn kind(&self) -> &'static str;
    fn signature(&self) -> Signature;
    fn default_timeout_ms(&self) -> u64 {
        60_000
    }
    async fn run(
        &self,
        ctx: &StepContext,
        inputs: &[&Artifact],
        params: &serde_json::Value,
    ) -> Result<StepOutput, StepError>;
}

/// All step implementations known to a worker.
#[derive(Default, Clone)]
pub struct StepRegistry {
    steps: HashMap<&'static str, Arc<dyn Step>>,
}

impl StepRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, step: Arc<dyn Step>) -> &mut Self {
        self.steps.insert(step.kind(), step);
        self
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn Step>> {
        self.steps.get(kind).cloned()
    }

    pub fn kinds(&self) -> impl Iterator<Item = &&'static str> {
        self.steps.keys()
    }
}

impl StepSignatures for StepRegistry {
    fn signature(&self, step_kind: &str) -> Option<Signature> {
        self.steps.get(step_kind).map(|s| s.signature())
    }
}
