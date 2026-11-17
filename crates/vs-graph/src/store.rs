//! What a store needs in order to persist one document run, beyond the graph itself.
//!
//! These live here, in the crate that owns `DocGraph`, so that no store's error type or
//! wire format leaks into the pipeline.
//!
//! There was a `GraphStore` trait here too, with one implementation that wrote the graph
//! tables of the vault and then the vectors. It is gone: the graph layer is written in
//! the vault's own Stage 2 transaction (`vs_vault::VaultWriter::write_stage2`) and the
//! vectors through `vs_vectors::VectorStore`, which is the seam that actually has two
//! possible backends.

use std::collections::{HashMap, HashSet};

use vs_core::{DocumentType, RunId, TenantId};
use vs_extract::resolve::Resolution;

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
    /// Every per-page type seen, for the record.
    pub doc_types: Vec<String>,
    /// The document's one type, once Stage 2 has settled it. `None` when there were no
    /// pages to judge from.
    pub doc_type: Option<DocumentType>,
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
            doc_type: None,
            new_entities: HashSet::new(),
        };
        let new = HashKey::entity(&run.tenant, "method|bert");
        run.note_resolution(&Resolution::New { entity_hk: new.clone() });
        run.note_resolution(&Resolution::Link { entity_hk: HashKey::entity(&run.tenant, "x"), score: 0.9 });
        assert_eq!(run.new_entities, HashSet::from([new.as_str().to_string()]));
    }
}
