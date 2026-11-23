//! Qdrant implementation of [`VectorStore`]. See `SPEC.md` for the collection layout.

use async_trait::async_trait;
use qdrant_client::qdrant::{
    Condition, CreateAliasBuilder, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder,
    Distance, FieldType, Filter, Fusion, HnswConfigDiffBuilder, KeywordIndexParamsBuilder, Modifier, PointStruct,
    PrefetchQueryBuilder, Query, QueryPointsBuilder, SparseVector, SparseVectorParamsBuilder,
    SparseVectorsConfigBuilder, UpsertPointsBuilder, Value, VectorInput, VectorParamsBuilder, VectorsConfigBuilder,
};
use qdrant_client::{Payload, Qdrant};

use vs_core::{HashKey, TenantId};
use vs_graph::{DocGraph, NodeKind, RunInfo, Vectors};

use crate::sparse;
use crate::{EntityHit, Hit, HybridQuery, Scope, VectorError, VectorStore};

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
    /// Whether the server applies inverse document frequency to the sparse half.
    ///
    /// `true` (the default) is the better ranking and the thing BM25 needs: the client
    /// sends term frequencies and Qdrant supplies IDF from the collection's own
    /// statistics. The collection holds every tenant's points, so those statistics are
    /// shared — a term's rarity is computed across organizations. No content crosses a
    /// tenant boundary (every query is filtered, and the filter is an index, not a
    /// post-pass), but a ranking that depends on other organizations' corpora is a
    /// statistical side channel, and a deployment where that is unacceptable sets this
    /// to `false` and gets saturated term frequencies alone: a weaker lexical ranking
    /// computed entirely from the document in front of it.
    ///
    /// It is a per-collection property, so it cannot be changed under live points — see
    /// `SPEC.md`.
    pub idf: bool,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:6334".into(),
            api_key: None,
            collection_prefix: "vs".into(),
            batch_size: 256,
            idf: true,
        }
    }
}

pub struct QdrantStore {
    client: Qdrant,
    blocks: String,
    sections: String,
    entities: String,
    batch_size: usize,
    idf: bool,
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
            idf: cfg.idf,
        })
    }

    fn collection_of(&self, scope: Scope) -> &str {
        match scope {
            Scope::Blocks => &self.blocks,
            Scope::Sections => &self.sections,
            Scope::Entities => &self.entities,
        }
    }

    fn collection_for(&self, kind: NodeKind) -> Option<&str> {
        match kind {
            NodeKind::Block => Some(&self.blocks),
            NodeKind::Section => Some(&self.sections),
            NodeKind::Entity => Some(&self.entities),
            _ => None,
        }
    }

    /// Creates one collection with a dense and a sparse named vector, the tenant index,
    /// and an alias so the configured name is not the collection's own name.
    ///
    /// The indirection is what makes a dimension change possible at all. A live
    /// collection's vector size cannot be altered, so the physical collection carries its
    /// dimension in its name (`vs_blocks__d1024`) and `vs_blocks` is an alias pointing at
    /// it: `migrate_dimensions` creates the next one and moves the alias, and every
    /// reader keeps using the configured name throughout.
    ///
    /// An **existing** collection under the configured name — a deployment that predates
    /// the alias — is left exactly as it is and used directly. Renaming it would be a
    /// migration nobody asked for, and its points are still perfectly good.
    async fn ensure_collection(&self, name: &str, dimensions: usize) -> Result<(), VectorError> {
        if self.client.collection_exists(name).await.map_err(unavailable)? {
            // Resolves through an alias as well, so this covers both layouts. The guard
            // matters because the alternative is silent: Qdrant refuses each point whose
            // vector is the wrong length, so a mismatch used to surface as every write
            // failing, one document at a time, with nothing saying why.
            if let Some(existing) = self.dimensions_of(name).await? {
                if existing != dimensions {
                    return Err(VectorError::Config(format!(
                        "collection {name} holds {existing}-dimensional vectors and this process embeds {dimensions}-dimensional ones; \
                         run `vs-suite vectors migrate` to create a collection for the new dimension (the documents then have to be re-run), \
                         or point [qdrant].collection_prefix at a different deployment"
                    )));
                }
            }
            return Ok(());
        }
        let physical = physical_name(name, dimensions);
        if !self.client.collection_exists(&physical).await.map_err(unavailable)? {
            self.create_collection(&physical, dimensions).await?;
        }
        self.alias(name, &physical).await?;
        Ok(())
    }

    /// The dense vector size of a collection, resolving aliases. `None` when the
    /// collection reports no dense vector configuration, which is not a shape this
    /// crate writes.
    async fn dimensions_of(&self, name: &str) -> Result<Option<usize>, VectorError> {
        use qdrant_client::qdrant::vectors_config::Config as VConfig;
        let info = self.client.collection_info(name).await.map_err(unavailable)?;
        let params = info.result.and_then(|r| r.config).and_then(|c| c.params);
        let Some(cfg) = params.and_then(|p| p.vectors_config).and_then(|v| v.config) else {
            return Ok(None);
        };
        Ok(match cfg {
            VConfig::Params(p) => Some(p.size as usize),
            VConfig::ParamsMap(m) => m.map.get(DENSE).map(|p| p.size as usize),
        })
    }

    /// Points `alias` at `collection`, replacing whatever it pointed at before.
    async fn alias(&self, alias: &str, collection: &str) -> Result<(), VectorError> {
        self.client
            .create_alias(CreateAliasBuilder::new(collection, alias))
            .await
            .map_err(|e| VectorError::Unavailable(format!("alias {alias} -> {collection}: {e}")))?;
        tracing::info!(alias, collection, "qdrant alias set");
        Ok(())
    }

    async fn create_collection(&self, name: &str, dimensions: usize) -> Result<(), VectorError> {
        let mut dense = VectorsConfigBuilder::default();
        dense.add_named_vector_params(DENSE, VectorParamsBuilder::new(dimensions as u64, Distance::Cosine).build());

        let mut sparse_cfg = SparseVectorsConfigBuilder::default();
        // `Modifier::Idf` makes the server apply inverse document frequency, so the
        // client only has to send term frequencies. This is what lets the sparse half
        // work without an embedding model — and what makes the statistics behind the
        // ranking collection-wide rather than per tenant, which `[qdrant].idf = false`
        // is there to opt out of.
        let mut params = SparseVectorParamsBuilder::default();
        if self.idf {
            params = params.modifier(Modifier::Idf);
        }
        sparse_cfg.add_named_vector_params(SPARSE, params.build());

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
        tracing::info!(collection = name, dimensions, idf = self.idf, "qdrant collection created");
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

/// The collection behind an alias, for a given dimension.
///
/// Two underscores so it cannot collide with a prefix a deployment chose: `vs_blocks`
/// and `vs__d768` are both plausible configured names, `vs_blocks__d768` is not.
fn physical_name(alias: &str, dimensions: usize) -> String {
    format!("{alias}__d{dimensions}")
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

    async fn search(&self, tenant: &TenantId, scope: Scope, q: &HybridQuery) -> Result<Vec<Hit>, VectorError> {
        if q.is_empty() {
            return Ok(vec![]);
        }
        let collection = self.collection_of(scope);
        let mut filter = vec![Condition::matches(TENANT, tenant.as_str().to_string())];
        if let Some(doc) = &q.document_hk {
            filter.push(Condition::matches(DOCUMENT_HK, doc.clone()));
        }
        let filter = Filter::must(filter);
        let limit = q.limit as u64;
        let prefetch = q.prefetch_limit() as u64;
        let dense = q.dense.as_ref().filter(|d| !d.is_empty());
        let sparse = q.text.as_deref().map(sparse::encode).filter(|s| !s.is_empty());

        let mut builder = QueryPointsBuilder::new(collection)
            .filter(filter.clone())
            .limit(limit)
            .with_payload(true)
            .with_vectors(q.with_vector);

        builder = match (dense, &sparse) {
            // Both halves: each is prefetched on its own index and the server fuses the
            // two candidate lists. Reciprocal rank fusion, because the two scores are
            // not comparable — a cosine similarity and a BM25 score have neither the
            // same scale nor the same distribution, so fusing by *rank* is the only
            // combination that needs no per-corpus tuning.
            (Some(d), Some(s)) => builder
                .add_prefetch(
                    PrefetchQueryBuilder::default()
                        .query(Query::new_nearest(d.to_vec()))
                        .using(DENSE)
                        .filter(filter.clone())
                        .limit(prefetch),
                )
                .add_prefetch(
                    PrefetchQueryBuilder::default()
                        .query(Query::new_nearest(VectorInput::new_sparse(s.indices.clone(), s.values.clone())))
                        .using(SPARSE)
                        .filter(filter)
                        .limit(prefetch),
                )
                .query(Query::new_fusion(Fusion::Rrf)),
            (Some(d), None) => builder.query(Query::new_nearest(d.to_vec())).using(DENSE),
            (None, Some(s)) => builder
                .query(Query::new_nearest(VectorInput::new_sparse(s.indices.clone(), s.values.clone())))
                .using(SPARSE),
            // `is_empty` already refused a query with neither half — but a query whose
            // text tokenised to nothing (`"(3)"`) lands here too, and it is not an error.
            (None, None) => return Ok(vec![]),
        };

        let response = self.client.query(builder).await.map_err(unavailable)?;
        let hits: Vec<Hit> = response
            .result
            .into_iter()
            .filter_map(|p| {
                let key = p.payload.get(KEY)?.as_str()?.to_string();
                Some(Hit {
                    key,
                    score: p.score,
                    document_hk: p.payload.get(DOCUMENT_HK).and_then(|v| v.as_str()).cloned(),
                    run_id: p.payload.get(RUN_ID).and_then(|v| v.as_str()).cloned(),
                    page_no: p.payload.get(PAGE_NO).and_then(|v| v.as_integer()),
                    type_id: p.payload.get(TYPE_ID).and_then(|v| v.as_str()).cloned(),
                    vector: dense_of(p.vectors),
                })
            })
            .collect();
        metrics::counter!("vectors_searches_total", "scope" => scope.as_str(), "mode" => mode_label(q)).increment(1);
        Ok(hits)
    }

    async fn migrate_dimensions(&self, dimensions: usize) -> Result<Vec<String>, VectorError> {
        if dimensions == 0 {
            return Err(VectorError::Config("embedding dimensions must be non-zero".into()));
        }
        let mut report = Vec::new();
        for alias in [&self.blocks, &self.sections, &self.entities] {
            let physical = physical_name(alias, dimensions);
            // A collection under the configured name that is *not* an alias cannot be
            // migrated by moving one: Qdrant will not create an alias that shadows a
            // real collection. Say so plainly rather than failing with its error.
            let aliases = self.client.list_collection_aliases(alias.as_str()).await.ok();
            let shadowed = self.client.collection_exists(alias).await.map_err(unavailable)? && aliases.is_none();
            if shadowed {
                return Err(VectorError::Config(format!(
                    "{alias} is a collection, not an alias: this deployment predates aliased collections. \
                     Rename or drop it first (its points are only recoverable by re-running the documents), \
                     or set [qdrant].collection_prefix to a new prefix and re-run."
                )));
            }
            if self.client.collection_exists(&physical).await.map_err(unavailable)? {
                report.push(format!("{physical} already exists"));
            } else {
                self.create_collection(&physical, dimensions).await?;
                report.push(format!("created {physical}"));
            }
            self.alias(alias, &physical).await?;
            report.push(format!("{alias} -> {physical}"));
        }
        report.push(
            "the previous collections are untouched; their vectors cannot be converted, so re-run the documents whose \
             embeddings you want searchable at the new dimension"
                .into(),
        );
        Ok(report)
    }
}

/// Which halves a query used, for the metric label.
fn mode_label(q: &HybridQuery) -> &'static str {
    match (q.dense.as_ref().is_some_and(|d| !d.is_empty()), q.text.as_deref().is_some_and(|t| !t.is_empty())) {
        (true, true) => "hybrid",
        (true, false) => "dense",
        (false, true) => "lexical",
        (false, false) => "empty",
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
