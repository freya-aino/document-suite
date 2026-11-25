//! Vector store for the suite's embeddings. See `SPEC.md`.
//!
//! The graph lives in Postgres (`ds-vault`); this crate holds only vectors plus the
//! payload needed to join a hit back to a graph node. The `VectorStore` trait is what the
//! rest of the suite depends on, so the backend stays replaceable.

pub mod qdrant;
pub mod sparse;

pub use qdrant::{QdrantConfig, QdrantStore};
pub use sparse::Sparse;

use async_trait::async_trait;

use ds_core::TenantId;
use ds_graph::{DocGraph, RunInfo, Vectors};

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


/// Which collection a search reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Block text: the passages a reader cites.
    Blocks,
    /// Section summaries: coarser, for finding the part of a document to read.
    Sections,
    /// Entity names, for corpus resolution.
    Entities,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Blocks => "blocks",
            Scope::Sections => "sections",
            Scope::Entities => "entities",
        }
    }
}

/// One hybrid query: a dense vector, the query text for the lexical half, or both.
///
/// Both halves are optional so the same call covers all three shapes a reader wants —
/// dense only, lexical only, or fused — without three methods that would each have to
/// agree about filtering and payloads.
#[derive(Debug, Clone, Default)]
pub struct HybridQuery {
    /// The embedded query. Omit for a purely lexical search.
    pub dense: Option<Vec<f32>>,
    /// The query as text, tokenised here into the same sparse space the points were
    /// written in. Omit for a purely dense search.
    pub text: Option<String>,
    /// Results wanted.
    pub limit: usize,
    /// Candidates each half contributes before fusion. Larger costs a little more and
    /// finds more of what only one half ranks highly; 0 means `4 × limit`.
    pub prefetch: usize,
    /// Restrict to one document, for "search within this document".
    pub document_hk: Option<String>,
    /// Return the stored vector with each hit. Needed by corpus resolution, which scores
    /// with its own cosine rather than trusting the store's metric; a plain search does
    /// not pay for it.
    pub with_vector: bool,
}

impl HybridQuery {
    /// A dense-only query.
    pub fn dense(vector: Vec<f32>, limit: usize) -> Self {
        Self { dense: Some(vector), limit, ..Default::default() }
    }

    /// A lexical-only query.
    pub fn text(text: impl Into<String>, limit: usize) -> Self {
        Self { text: Some(text.into()), limit, ..Default::default() }
    }

    /// Both halves, fused.
    pub fn hybrid(vector: Vec<f32>, text: impl Into<String>, limit: usize) -> Self {
        Self { dense: Some(vector), text: Some(text.into()), limit, ..Default::default() }
    }

    pub fn within(mut self, document_hk: impl Into<String>) -> Self {
        self.document_hk = Some(document_hk.into());
        self
    }

    pub fn with_vector(mut self) -> Self {
        self.with_vector = true;
        self
    }

    /// Candidates per half.
    pub fn prefetch_limit(&self) -> usize {
        if self.prefetch > 0 {
            self.prefetch
        } else {
            self.limit.saturating_mul(4).max(1)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.limit == 0 || (self.dense.as_ref().is_none_or(|d| d.is_empty()) && self.text.as_deref().is_none_or(str::is_empty))
    }
}

/// One hit from a search.
#[derive(Debug, Clone)]
pub struct Hit {
    /// The graph key: `block_hk`, `section_key` or `entity_hk`, which is what joins this
    /// back to Postgres.
    pub key: String,
    /// The store's score. **Not comparable across queries**: for a fused query it is a
    /// reciprocal-rank score, not a similarity, which is why anything with a threshold
    /// (corpus resolution) scores the returned `vector` itself instead.
    pub score: f32,
    pub document_hk: Option<String>,
    pub run_id: Option<String>,
    pub page_no: Option<i64>,
    pub type_id: Option<String>,
    pub vector: Option<Vec<f32>>,
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

    /// Dense, lexical, or both fused, within one tenant.
    ///
    /// The lexical half was written from the first version of this crate and queried by
    /// nothing, which made it stored and unproven: half a retrieval design that no test
    /// could exercise. Fusion is here rather than left to the reader because *where* it
    /// happens decides whether it works at all — the two candidate sets have to be fused
    /// inside the store, in one request, or the client sees two top-k lists and cannot
    /// recover the ranking it would have got from the union.
    async fn search(&self, tenant: &TenantId, scope: Scope, query: &HybridQuery) -> Result<Vec<Hit>, VectorError>;

    /// Creates the collection a dimension change needs and points the configured name at
    /// it, leaving the old one in place.
    ///
    /// A deliberate operation, not something `ensure_ready` does on its own: the vectors
    /// in the old collection cannot be converted — a different embedder means different
    /// geometry — so the documents have to be re-run. Returns what it did, for an
    /// operator to read.
    async fn migrate_dimensions(&self, dimensions: usize) -> Result<Vec<String>, VectorError>;
}
