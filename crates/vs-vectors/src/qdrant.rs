//! Qdrant implementation of [`VectorStore`]. See `SPEC.md` for the collection layout.

use async_trait::async_trait;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder, Distance, FieldType,
    Filter, HnswConfigDiffBuilder, KeywordIndexParamsBuilder, Modifier, PointStruct, Query, QueryPointsBuilder,
    SparseVector, SparseVectorParamsBuilder, SparseVectorsConfigBuilder, UpsertPointsBuilder, Value,
    VectorParamsBuilder, VectorsConfigBuilder,
};
use qdrant_client::{Payload, Qdrant};

use vs_core::{HashKey, TenantId};
use vs_graph::{DocGraph, NodeKind, RunInfo, Vectors};

use crate::sparse;
use crate::{EntityHit, VectorError, VectorStore};

/// Named vectors on every point. Both names are part of the stored data: renaming one
/// makes existing collections unqueryable.
const DENSE: &str = "dense";
const SPARSE: &str = "sparse";

/// The payload field every query filters on. Indexed with `is_tenant`.
const TENANT: &str = "tenant";
/// The graph key this vector belongs to — `block_hk`, `section_key` or `entity_hk`.
/// Point ids are UUIDs (Qdrant accepts only integers or UUIDs), so the real key has to
/// live in the payload to get from a hit back to the graph.
const KEY: &str = "key";
const DOCUMENT_HK: &str = "document_hk";
const RUN_ID: &str = "run_id";
const MODEL_REF: &str = "model_ref";
const TYPE_ID: &str = "type_id";
const PAGE_NO: &str = "page_no";

#[derive(Debug, Clone)]
pub struct QdrantConfig {
    /// gRPC endpoint, e.g. `http://127.0.0.1:6334`.
    pub url: String,
    pub api_key: Option<String>,
    /// Prefixed onto every collection name, so one Qdrant can host several deployments.
    pub collection_prefix: String,
    /// Points per upsert request.
    pub batch_size: usize,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self { url: "http://127.0.0.1:6334".into(), api_key: None, collection_prefix: "vs".into(), batch_size: 256 }
    }
}

pub struct QdrantStore {
    client: Qdrant,
    blocks: String,
    sections: String,
    entities: String,
    batch_size: usize,
}

fn unavailable(e: impl std::fmt::Display) -> VectorError {
    VectorError::Unavailable(e.to_string())
}

impl QdrantStore {
    pub fn new(cfg: &QdrantConfig) -> Result<Self, VectorError> {
        let mut b = Qdrant::from_url(&cfg.url);
        if let Some(key) = cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
            b = b.api_key(key.to_string());
        }
        let client = b.build().map_err(|e| VectorError::Config(format!("qdrant {}: {e}", cfg.url)))?;
        let p = &cfg.collection_prefix;
        Ok(Self {
            client,
            blocks: format!("{p}_blocks"),
            sections: format!("{p}_sections"),
            entities: format!("{p}_entities"),
            batch_size: cfg.batch_size.max(1),
        })
    }

    fn collection_for(&self, kind: NodeKind) -> Option<&str> {
        match kind {
            NodeKind::Block => Some(&self.blocks),
            NodeKind::Section => Some(&self.sections),
            NodeKind::Entity => Some(&self.entities),
            _ => None,
        }
    }

    /// Creates one collection with a dense and a sparse named vector, and the tenant
    /// index. Existing collections are left alone — the dimension of a live collection
    /// cannot be changed, so a dimension change is a new collection (see `SPEC.md`).
    async fn ensure_collection(&self, name: &str, dimensions: usize) -> Result<(), VectorError> {
        if self.client.collection_exists(name).await.map_err(unavailable)? {
            return Ok(());
        }
        let mut dense = VectorsConfigBuilder::default();
        dense.add_named_vector_params(DENSE, VectorParamsBuilder::new(dimensions as u64, Distance::Cosine).build());

        let mut sparse_cfg = SparseVectorsConfigBuilder::default();
        // `Modifier::Idf` makes the server apply inverse document frequency, so the
        // client only has to send term frequencies. This is what lets the sparse half
        // work without an embedding model.
        sparse_cfg.add_named_vector_params(SPARSE, SparseVectorParamsBuilder::default().modifier(Modifier::Idf).build());

        let create = CreateCollectionBuilder::new(name)
            .vectors_config(dense)
            .sparse_vectors_config(sparse_cfg)
            // m = 0 disables the global HNSW graph; payload_m builds one sub-graph per
            // tenant instead. Only correct because every query filters by tenant.
            .hnsw_config(HnswConfigDiffBuilder::default().m(0).payload_m(16).build());

        match self.client.create_collection(create).await {
            Ok(_) => {}
            // Another worker won the race; that is the desired end state either way.
            Err(e) if self.client.collection_exists(name).await.unwrap_or(false) => {
                tracing::debug!(collection = name, error = %e, "collection already created concurrently");
                return self.ensure_tenant_index(name).await;
            }
            Err(e) => return Err(unavailable(e)),
        }
        self.ensure_tenant_index(name).await?;
        tracing::info!(collection = name, dimensions, "qdrant collection created");
        Ok(())
    }

    async fn ensure_tenant_index(&self, name: &str) -> Result<(), VectorError> {
        let index = CreateFieldIndexCollectionBuilder::new(name, TENANT, FieldType::Keyword)
            .field_index_params(KeywordIndexParamsBuilder::default().is_tenant(true).build());
        // Already-exists is reported as an error; the end state is what matters.
        if let Err(e) = self.client.create_field_index(index).await {
            tracing::debug!(collection = name, error = %e, "tenant index already present or not created");
        }
        Ok(())
    }

    async fn upsert(&self, collection: &str, points: Vec<PointStruct>) -> Result<(), VectorError> {
        for chunk in points.chunks(self.batch_size) {
            self.client
                .upsert_points(UpsertPointsBuilder::new(collection, chunk.to_vec()).wait(true))
                .await
                .map_err(unavailable)?;
        }
        Ok(())
    }

    async fn delete_where(&self, collection: &str, filter: Filter) -> Result<(), VectorError> {
        self.client
            .delete_points(DeletePointsBuilder::new(collection).points(filter).wait(true))
            .await
            .map_err(unavailable)?;
        Ok(())
    }
}

/// Qdrant point ids must be integers or UUIDs, and our keys are neither: a `block_hk` is
/// 64 hex chars and a section key looks like `sec:<hash>`. Hashing the key and taking 128
/// bits of it gives a deterministic UUID, so the same key always lands on the same point
/// and an upsert of an unchanged run overwrites in place rather than duplicating.
fn point_id(key: &str) -> String {
    let h = HashKey::of(&[key]);
    let hex = h.as_str();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

fn payload(pairs: Vec<(&str, Value)>) -> Payload {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<std::collections::HashMap<_, _>>().into()
}

fn tenant_filter(tenant: &TenantId) -> Filter {
    Filter::must([Condition::matches(TENANT, tenant.as_str().to_string())])
}

#[async_trait]
impl VectorStore for QdrantStore {
    async fn ensure_ready(&self, dimensions: usize) -> Result<(), VectorError> {
        if dimensions == 0 {
            return Err(VectorError::Config("embedding dimensions must be non-zero".into()));
        }
        for name in [&self.blocks, &self.sections, &self.entities] {
            self.ensure_collection(name, dimensions).await?;
        }
        Ok(())
    }

    async fn write_run(&self, run: &RunInfo, graph: &DocGraph, vectors: &Vectors) -> Result<(), VectorError> {
        let document_hk = graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Document)
            .map(|n| n.key.clone())
            .ok_or_else(|| VectorError::Rejected("graph has no document node".into()))?;
        let run_id = run.run_id.to_string();
        let tenant = run.tenant.as_str().to_string();

        // A retry of this run must not leave the previous attempt's points behind: a
        // block that vanished from the new attempt would otherwise stay searchable.
        self.drop_run(&run.tenant, &document_hk, &run_id).await?;

        let page_of: std::collections::HashMap<&str, i64> = graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Block)
            .filter_map(|n| n.props.get("page_no").and_then(|p| p.as_i64()).map(|p| (n.key.as_str(), p)))
            .collect();
        let type_of: std::collections::HashMap<&str, String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Entity)
            .map(|n| {
                let t = n.props.get("type_id").and_then(|v| v.as_str()).unwrap_or_default();
                (n.key.as_str(), t.to_string())
            })
            .collect();

        let (mut blocks, mut sections, mut entities) = (Vec::new(), Vec::new(), Vec::new());
        for t in &graph.embed_targets {
            let Some(dense) = vectors.get(&t.node_key) else { continue };
            // An entity resolution found is already stored with the vector every other
            // document was matched against; rewriting it would move the target.
            if t.kind == NodeKind::Entity && !run.new_entities.contains(&t.node_key) {
                continue;
            }
            let mut fields = vec![
                (TENANT, Value::from(tenant.clone())),
                (KEY, Value::from(t.node_key.clone())),
                (MODEL_REF, Value::from(vectors.model_ref.clone())),
            ];
            match t.kind {
                NodeKind::Entity => fields.push((TYPE_ID, Value::from(type_of.get(t.node_key.as_str()).cloned().unwrap_or_default()))),
                _ => {
                    fields.push((DOCUMENT_HK, Value::from(document_hk.clone())));
                    fields.push((RUN_ID, Value::from(run_id.clone())));
                    if let Some(p) = page_of.get(t.node_key.as_str()) {
                        fields.push((PAGE_NO, Value::from(*p)));
                    }
                }
            }
            let s = sparse::encode(&t.text);
            let mut named: std::collections::HashMap<String, qdrant_client::qdrant::Vector> =
                std::collections::HashMap::from([(DENSE.to_string(), dense.to_vec().into())]);
            // A text with no indexable token (a lone figure number, say) gets no sparse
            // vector at all rather than an empty one, which Qdrant rejects.
            if !s.is_empty() {
                named.insert(SPARSE.to_string(), SparseVector::from((s.indices, s.values)).into());
            }
            let point = PointStruct::new(point_id(&t.node_key), named, payload(fields));
            match t.kind {
                NodeKind::Block => blocks.push(point),
                NodeKind::Section => sections.push(point),
                NodeKind::Entity => entities.push(point),
                _ => {}
            }
        }

        let counts = (blocks.len(), sections.len(), entities.len());
        for (kind, points) in
            [(NodeKind::Block, blocks), (NodeKind::Section, sections), (NodeKind::Entity, entities)]
        {
            let collection = self
                .collection_for(kind)
                .ok_or_else(|| VectorError::Rejected(format!("no collection for {kind:?}")))?
                .to_string();
            self.upsert(&collection, points).await?;
        }
        tracing::info!(%run_id, blocks = counts.0, sections = counts.1, entities = counts.2, "vectors written");
        Ok(())
    }

    async fn drop_run(&self, tenant: &TenantId, document_hk: &str, run_id: &str) -> Result<(), VectorError> {
        let filter = Filter::must([
            Condition::matches(TENANT, tenant.as_str().to_string()),
            Condition::matches(DOCUMENT_HK, document_hk.to_string()),
            Condition::matches(RUN_ID, run_id.to_string()),
        ]);
        self.delete_where(&self.blocks, filter.clone()).await?;
        self.delete_where(&self.sections, filter).await?;
        Ok(())
    }

    async fn search_entities(&self, tenant: &TenantId, vector: &[f32], k: usize) -> Result<Vec<EntityHit>, VectorError> {
        if vector.is_empty() || k == 0 {
            return Ok(vec![]);
        }
        // Qdrant filters before the ANN search, so `k` is `k` — unlike the previous
        // store, which post-filtered by tenant and therefore had to over-fetch 4x.
        let q = QueryPointsBuilder::new(&self.entities)
            .query(Query::new_nearest(vector.to_vec()))
            .using(DENSE)
            .filter(tenant_filter(tenant))
            .limit(k as u64)
            .with_payload(true)
            .with_vectors(true);
        let response = self.client.query(q).await.map_err(unavailable)?;
        Ok(response
            .result
            .into_iter()
            .filter_map(|p| {
                let key = p.payload.get(KEY)?.as_str()?.to_string();
                let type_id = p.payload.get(TYPE_ID).and_then(|v| v.as_str()).cloned().unwrap_or_default();
                let vector = dense_of(p.vectors);
                Some(EntityHit { entity_hk: key, type_id, vector })
            })
            .collect())
    }
}

/// Pulls the `dense` vector out of a query result. A hit without one is not an error:
/// the caller re-embeds the candidate's name instead, which is also what happens when a
/// vector was written by a different embedding model.
fn dense_of(vectors: Option<qdrant_client::qdrant::VectorsOutput>) -> Option<Vec<f32>> {
    use qdrant_client::qdrant::vector_output::Vector as OutVector;
    use qdrant_client::qdrant::vectors_output::VectorsOptions;
    let one = match vectors?.vectors_options? {
        VectorsOptions::Vector(v) => v,
        VectorsOptions::Vectors(mut named) => named.vectors.remove(DENSE)?,
    };
    match one.into_vector() {
        OutVector::Dense(d) => Some(d.data),
        // A sparse or multi-vector here means the collection layout changed under us.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_id_is_a_deterministic_uuid() {
        let hk = "a".repeat(64);
        let id = point_id(&hk);
        assert_eq!(id, point_id(&hk), "same key must map to the same point");
        assert_ne!(id, point_id("sec:other"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // Must parse as a UUID, or Qdrant rejects the point.
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn collection_names_are_prefixed() {
        let cfg = QdrantConfig { collection_prefix: "acme".into(), ..Default::default() };
        let s = QdrantStore::new(&cfg).unwrap();
        assert_eq!(s.collection_for(NodeKind::Block), Some("acme_blocks"));
        assert_eq!(s.collection_for(NodeKind::Section), Some("acme_sections"));
        assert_eq!(s.collection_for(NodeKind::Entity), Some("acme_entities"));
        assert_eq!(s.collection_for(NodeKind::Assertion), None);
    }
}
