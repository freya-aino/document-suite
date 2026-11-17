use serde::{Deserialize, Serialize};

use crate::{DocType, HashKey, LanguageTag, RunId, TenantId};

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

/// Who asked for a job, from the verified token — never from a request body.
///
/// The isolation boundary is the organization, which is the `tenant` and is hashed into
/// every key (`HashKey::document`). This is the rest of the caller's identity, recorded
/// so that read scoping *within* an organization can be enforced later without
/// re-plumbing: a job already knows which person, department and workspaces it came
/// from. Nothing enforces on it yet — see `vs-api/SPEC.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// Keycloak `sub`: stable, and not an email address.
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub department: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
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
    /// Document type, when the caller already knows it.
    ///
    /// A caller uploading a batch of invoices knows more than a classifier guessing from
    /// pixels, so this is trusted over both the per-page vote and the document
    /// classifier. It also reaches **page** classification, where it is the only way a
    /// page can be told what document it belongs to: pages are classified in parallel
    /// across workers with no shared per-document state, so nothing else can tell page 7
    /// what page 1 concluded.
    #[serde(default)]
    pub doc_type: Option<DocType>,
    /// The authenticated caller. `None` only for specs written before authentication
    /// existed, which is why it is `#[serde(default)]`: `ops.jobs.spec` is JSONB and a
    /// job queued by the previous build must still deserialize.
    #[serde(default)]
    pub owner: Option<Owner>,
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
