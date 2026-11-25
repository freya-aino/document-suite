//! The production step registry.
//!
//! One place that lists every step a `page` worker can run, so the worker, `ds-api`
//! (DAG-list upload validation) and `ds-adapt` (candidate validation) agree on the
//! catalogue. Add new steps here, nowhere else.

use std::sync::Arc;

use ds_dag::StepRegistry;
use ds_models::OpenAiClient;

/// All steps a production worker knows. DAG lists are validated against this.
pub fn production_registry(client: Arc<OpenAiClient>) -> StepRegistry {
    let mut reg = StepRegistry::new();
    reg.register(Arc::new(ds_ingest::steps::MergeNative))
        .register(Arc::new(ds_ingest::steps::MergeBlocks))
        .register(Arc::new(ds_ingest::steps::VerifyBlocks))
        .register(Arc::new(ds_ingest::steps::FigureCrop))
        // Pre-processing. Steps rather than a fixed pass so `ds-adapt` can measure
        // whether each one helps a given page class instead of assuming it does.
        .register(Arc::new(ds_ingest::preprocess::Orient))
        .register(Arc::new(ds_ingest::preprocess::Deskew))
        .register(Arc::new(ds_ingest::preprocess::Denoise))
        .register(Arc::new(ds_ingest::preprocess::Normalize))
        .register(Arc::new(ds_ingest::preprocess::Binarize))
        .register(Arc::new(ds_ingest::preprocess::CropMargins));
    ds_models::register_steps(&mut reg, client);
    reg
}
