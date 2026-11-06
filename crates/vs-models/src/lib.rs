//! Model access. See `SPEC.md`.

pub mod client;
pub mod classifier;
pub mod parsers;
pub mod prompts;
pub mod registry;
pub mod steps;

pub use client::{ChatRequest, Embedder, ModelError, OpenAiClient};
pub use registry::{ModelRegistry, ModelSpec};

use std::sync::Arc;
use vs_dag::StepRegistry;

/// Registers all model-backed steps.
pub fn register_steps(reg: &mut StepRegistry, client: Arc<OpenAiClient>) {
    use steps::*;
    reg.register(Arc::new(PageParseE2e { client: client.clone() }))
        .register(Arc::new(LayoutDetect { client: client.clone() }))
        .register(Arc::new(RegionStep::ocr(client.clone())))
        .register(Arc::new(RegionStep::table(client.clone())))
        .register(Arc::new(RegionStep::formula(client.clone())))
        .register(Arc::new(FigureDescribe { client: client.clone() }))
        .register(Arc::new(ChartToTable { client }));
}
