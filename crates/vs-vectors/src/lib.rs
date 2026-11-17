//! Vector store for the suite's embeddings. See `SPEC.md`.
//!
//! The graph lives in Postgres (`vs-vault`); this crate holds only vectors plus the
//! payload needed to join a hit back to a graph node. The `VectorStore` trait is what the
//! rest of the suite depends on, so the backend stays replaceable.

pub mod qdrant;
pub mod sparse;

pub use qdrant::{QdrantConfig, QdrantStore};
pub use sparse::Sparse;

use async_trait::async_trait;

use vs_core::TenantId;
use vs_graph::{DocGraph, RunInfo, Vectors};

#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("vector store unavailable: {0}")]
    Unavailable(String),
    #[error("vector store rejected the write: {0}")]
    Rejected(String),
    #[error("vector store misconfigured: {0}")]
    Config(String),
}

impl VectorError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, VectorError::Unavailable(_))
    }
}


/// One candidate from an entity-name search.
///
/// `vector` is the stored embedding, returned so the caller scores with its own cosine
/// rather than trusting the store's distance metric. That is not pedantry: the score a
/// vector store reports is index- and metric-dependent, and corpus resolution's threshold
/// (0.92 by default) is only meaningful against a known definition of similarity.
#[derive(Debug, Clone)]
pub struct EntityHit {
    pub entity_hk: String,
    pub type_id: String,
    pub vector: Option<Vec<f32>>,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Creates the collections if they are absent, sized for `dimensions`. Idempotent;
    /// called at worker startup.
    async fn ensure_ready(&self, dimensions: usize) -> Result<(), VectorError>;

    /// Replaces this run's block and section vectors and adds vectors for the entities
    /// this run created. Must be idempotent: delivery is at-least-once (system spec
    /// §5.4), so this is called again after any failure downstream of it with identical
    /// input, and writing the same run twice must leave the store as if written once.
    async fn write_run(&self, run: &RunInfo, graph: &DocGraph, vectors: &Vectors) -> Result<(), VectorError>;

    /// Removes one run's block and section vectors. Entity vectors are **not** touched:
    /// they outlive the run and other documents resolve against them.
    async fn drop_run(&self, tenant: &TenantId, document_hk: &str, run_id: &str) -> Result<(), VectorError>;

    /// Nearest entity names within one tenant, for corpus resolution.
    async fn search_entities(&self, tenant: &TenantId, vector: &[f32], k: usize) -> Result<Vec<EntityHit>, VectorError>;
}
