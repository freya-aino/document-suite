//! HelixDB writer. See `SPEC.md`.

use async_trait::async_trait;
use std::collections::HashMap;

use vs_core::{HashKey, RunId, TenantId};
use vs_extract::resolve::{CorpusResolver, Resolution};
use vs_extract::{DocEntity, ExtractError};
use vs_graph::DocGraph;

#[derive(Debug, thiserror::Error)]
pub enum HelixError {
    #[error("http: {0}")]
    Http(String),
    #[error("helix returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

/// Embeddings keyed by `EmbedTarget::node_key`.
pub struct Vectors {
    pub model_ref: String,
    pub dimensions: usize,
    pub by_key: HashMap<String, Vec<f32>>,
}

#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Writes one document run and makes it current (SPEC.md "Write algorithm").
    async fn write_run(&self, tenant: &TenantId, run: RunId, graph: &DocGraph, vectors: &Vectors) -> Result<(), HelixError>;
}

pub struct HelixStore {
    pub http: reqwest::Client,
    pub base_url: String,
    pub batch_size: usize,
}

#[async_trait]
impl GraphStore for HelixStore {
    async fn write_run(&self, _tenant: &TenantId, _run: RunId, graph: &DocGraph, _vectors: &Vectors) -> Result<(), HelixError> {
        let _batches = graph.nodes.chunks(self.batch_size.max(1)).count();
        Err(HelixError::NotImplemented("build SDK queries: upsert entities, insert run nodes/edges/vectors, switch current run, delete old run"))
    }
}

pub struct HelixResolver {
    pub store: std::sync::Arc<HelixStore>,
    pub link_threshold: f32,
    pub judge_band: (f32, f32),
}

#[async_trait]
impl CorpusResolver for HelixResolver {
    async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, _context: &str) -> Result<Resolution, ExtractError> {
        // v1 placeholder: no corpus lookup yet → deterministic new key (same name+type → same entity).
        let type_id = entity.type_id.as_deref().unwrap_or(&entity.raw_type);
        let key = format!("{}|{}", type_id, normalise_name(&entity.name));
        Ok(Resolution::New { entity_hk: HashKey::entity(tenant, &key) })
    }
}

pub fn normalise_name(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_normalisation() {
        assert_eq!(normalise_name("  Vision   Transformer "), "vision transformer");
    }
}
