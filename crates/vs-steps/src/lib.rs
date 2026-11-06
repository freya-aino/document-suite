//! The production step registry.
//!
//! One place that lists every step a `page` worker can run, so the worker, `vs-api`
//! (DAG-list upload validation) and `vs-adapt` (candidate validation) agree on the
//! catalogue. Add new steps here, nowhere else.

use std::sync::Arc;

use vs_dag::StepRegistry;
use vs_models::OpenAiClient;

/// All steps a production worker knows. DAG lists are validated against this.
pub fn production_registry(client: Arc<OpenAiClient>) -> StepRegistry {
    let mut reg = StepRegistry::new();
    reg.register(Arc::new(vs_ingest::steps::MergeNative))
        .register(Arc::new(vs_ingest::steps::MergeBlocks))
        .register(Arc::new(vs_ingest::steps::FigureCrop));
    vs_models::register_steps(&mut reg, client);
    reg
}
