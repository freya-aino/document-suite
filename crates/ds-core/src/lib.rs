//! Shared domain types for document-suite. See `README.md`.

pub mod block;
pub mod geometry;
pub mod ids;
pub mod job;
pub mod page;
pub mod provenance;

pub use block::{Block, BlockContent, BlockKind, PageResult};
pub use geometry::BBox;
pub use ids::{content_sha256, split_ref, HashKey, LanguageTag, RunId, TenantId};
pub use job::{JobSpec, JobState, Owner, Priority, Profile, Task};
pub use page::{ClassifierTier, DocType, PageProfile, DocTypeSource, DocumentType};
pub use provenance::{Evidence, Provenance};

/// Errors produced by validation of core types.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid bounding box: {0}")]
    InvalidBBox(String),
    #[error("invalid language tag: {0:?}")]
    InvalidLanguage(String),
}
