//! What a store needs in order to persist one document run, and the trait it implements.
//!
//! These live here, in the crate that owns `DocGraph`, so that no store's error type or
//! wire format leaks into the pipeline. `vs-worker` holds an `Arc<dyn GraphStore>` and
//! never names a backend.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use vs_core::{RunId, TenantId};
use vs_extract::resolve::Resolution;

use crate::DocGraph;

/// A store write failed. The distinction is the only thing the caller needs: a
/// transient failure is retried with the whole task, a rejection never will be.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store could not be reached, or failed in a way that may succeed later.
    #[error("graph store unavailable: {0}")]
    Unavailable(String),
    /// The store rejected the data. Retrying sends the same bytes, so it will fail again.
    #[error("graph store rejected the write: {0}")]
    Rejected(String),
}

impl StoreError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, StoreError::Unavailable(_))
    }
}

/// Embeddings for the graph's `EmbedTarget`s, keyed by `EmbedTarget::node_key` — which is
/// a `block_hk`, a `section_key` or an `entity_hk`, so a vector always resolves back to a
/// node without a side table.
pub struct Vectors {
    pub model_ref: String,
    pub dimensions: usize,
    pub by_key: HashMap<String, Vec<f32>>,
}

impl Vectors {
    pub fn get(&self, key: &str) -> Option<&[f32]> {
        self.by_key.get(key).map(|v| v.as_slice())
    }
}

/// What the graph does not carry about the run it came from.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub tenant: TenantId,
    pub run_id: RunId,
    pub language: String,
    pub page_count: u32,
    pub doc_types: Vec<String>,
    /// Entities that corpus resolution created in this run (`Resolution::New`).
    ///
    /// Only these need their name vector written: a `Link` resolved to an entity that is
    /// already stored and already has one. Writing it again would be wasted work, and in
    /// a store where the entity vector outlives the run it would also be wrong — the
    /// stored vector is what every other document was matched against.
    pub new_entities: HashSet<String>,
}

impl RunInfo {
    /// Records a corpus-resolution result. `New` joins `new_entities`; `Link` changes
    /// nothing, because that entity keeps the vector it was found by.
    pub fn note_resolution(&mut self, resolution: &Resolution) {
        if let Resolution::New { entity_hk } = resolution {
            self.new_entities.insert(entity_hk.as_str().to_string());
        }
    }
}

#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Writes one document run and makes it the current one.
    ///
    /// Must be **idempotent**: delivery is at-least-once (system spec §5.4), so this is
    /// called again after any failure downstream of it, with identical input. Writing the
    /// same run twice must leave the store as if it had been written once, and must not
    /// disturb rows shared with other runs or documents — the document itself, and any
    /// entity other documents also link to.
    async fn write_run(&self, run: &RunInfo, graph: &DocGraph, vectors: &Vectors) -> Result<(), StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::HashKey;

    /// A linked entity already has the vector it was found by; only a new one needs one
    /// written. Getting this wrong either wastes writes or overwrites the vector that
    /// every other document in the corpus was matched against.
    #[test]
    fn run_info_notes_new_entities_only() {
        let mut run = RunInfo {
            tenant: TenantId("t".into()),
            run_id: RunId(uuid::Uuid::nil()),
            language: "en".into(),
            page_count: 0,
            doc_types: vec![],
            new_entities: HashSet::new(),
        };
        let new = HashKey::entity(&run.tenant, "method|bert");
        run.note_resolution(&Resolution::New { entity_hk: new.clone() });
        run.note_resolution(&Resolution::Link { entity_hk: HashKey::entity(&run.tenant, "x"), score: 0.9 });
        assert_eq!(run.new_entities, HashSet::from([new.as_str().to_string()]));
    }

    #[test]
    fn store_error_retryability() {
        assert!(StoreError::Unavailable("timeout".into()).is_retryable());
        assert!(!StoreError::Rejected("bad bbox".into()).is_retryable());
    }
}
