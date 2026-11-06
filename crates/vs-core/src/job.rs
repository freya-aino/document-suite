use serde::{Deserialize, Serialize};

use crate::{HashKey, LanguageTag, RunId, TenantId};

/// Queue lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Small batches; reserved worker share and shorter timeouts.
    Interactive,
    #[default]
    Batch,
}

/// Caller preference used by the router to pick among candidate DAGs for a page class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Quality,
    #[default]
    Balanced,
    Fast,
}

/// Per-document request accepted by the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub tenant: TenantId,
    /// Required in v1; applies to all pages.
    pub language: LanguageTag,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub profile: Profile,
    /// Pin a DAG-list version for reproducibility; default = active list.
    pub dag_list_version: Option<String>,
    /// Object-store URI of the original file.
    pub source_uri: String,
    /// Caller's own reference, stored as a satellite attribute.
    pub external_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Received,
    Ingesting,
    PagesProcessing,
    Stitching,
    DocProcessing,
    Writing,
    Done,
    /// Done, but some pages failed (within the allowed threshold).
    Partial,
    Failed,
}

/// Task messages carried on the bus. Payloads are references, never page images.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "task", rename_all = "snake_case")]
pub enum Task {
    Ingest { job_id: uuid::Uuid, run_id: RunId, spec: JobSpec },
    Page {
        job_id: uuid::Uuid,
        run_id: RunId,
        document_hk: HashKey,
        page_no: u32,
        page_image_uri: String,
        text_layer_uri: Option<String>,
        spec: JobSpec,
    },
    Doc { job_id: uuid::Uuid, run_id: RunId, document_hk: HashKey, spec: JobSpec },
}
