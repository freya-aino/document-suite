//! Ontology access and embedding-based type normalisation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vs_models::Embedder;

use crate::resolve::{slug, TypeNormalizer};
use crate::ExtractError;

/// One entity or relation type of the ontology (`registry.entity_types` / `relation_types`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OntologyType {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
    /// `approved` | `pending` | `rejected`
    pub review_status: String,
}

/// Storage of the ontology (implemented over Postgres by the worker; in memory here).
#[async_trait]
pub trait OntologyStore: Send + Sync {
    async fn entity_types(&self) -> Result<Vec<OntologyType>, ExtractError>;
    async fn relation_types(&self) -> Result<Vec<OntologyType>, ExtractError>;
    async fn insert_pending_entity_type(&self, id: &str, label: &str) -> Result<(), ExtractError>;
    async fn insert_pending_relation_type(&self, id: &str, label: &str, roles: &[String]) -> Result<(), ExtractError>;
}

#[async_trait]
impl<S: OntologyStore + ?Sized> OntologyStore for Arc<S> {
    async fn entity_types(&self) -> Result<Vec<OntologyType>, ExtractError> {
        (**self).entity_types().await
    }
    async fn relation_types(&self) -> Result<Vec<OntologyType>, ExtractError> {
        (**self).relation_types().await
    }
    async fn insert_pending_entity_type(&self, id: &str, label: &str) -> Result<(), ExtractError> {
        (**self).insert_pending_entity_type(id, label).await
    }
    async fn insert_pending_relation_type(&self, id: &str, label: &str, roles: &[String]) -> Result<(), ExtractError> {
        (**self).insert_pending_relation_type(id, label, roles).await
    }
}

/// In-memory ontology (tests, single-process use).
#[derive(Debug, Default)]
pub struct InMemoryOntologyStore {
    pub entity_types: Mutex<Vec<OntologyType>>,
    pub relation_types: Mutex<Vec<(OntologyType, Vec<String>)>>,
}

impl InMemoryOntologyStore {
    pub fn with_types(entity_types: Vec<OntologyType>, relation_types: Vec<OntologyType>) -> Self {
        Self { entity_types: Mutex::new(entity_types), relation_types: Mutex::new(relation_types.into_iter().map(|t| (t, vec![])).collect()) }
    }
}

#[async_trait]
impl OntologyStore for InMemoryOntologyStore {
    async fn entity_types(&self) -> Result<Vec<OntologyType>, ExtractError> {
        Ok(self.entity_types.lock().unwrap().clone())
    }
    async fn relation_types(&self) -> Result<Vec<OntologyType>, ExtractError> {
        Ok(self.relation_types.lock().unwrap().iter().map(|(t, _)| t.clone()).collect())
    }
    async fn insert_pending_entity_type(&self, id: &str, label: &str) -> Result<(), ExtractError> {
        let mut g = self.entity_types.lock().unwrap();
        if !g.iter().any(|t| t.id == id) {
            g.push(OntologyType { id: id.into(), label: label.into(), synonyms: vec![], review_status: "pending".into() });
        }
        Ok(())
    }
    async fn insert_pending_relation_type(&self, id: &str, label: &str, roles: &[String]) -> Result<(), ExtractError> {
        let mut g = self.relation_types.lock().unwrap();
        if !g.iter().any(|(t, _)| t.id == id) {
            g.push((OntologyType { id: id.into(), label: label.into(), synonyms: vec![], review_status: "pending".into() }, roles.to_vec()));
        }
        Ok(())
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Entity,
    Relation,
}

/// Maps raw types/predicates to ontology ids by embedding similarity against each type's label
/// and synonyms (cosine ≥ `threshold`); exact slug matches short-circuit. No match → a pending
/// type with `id = slug(raw)` is created. Embeddings and decisions are cached per instance.
pub struct EmbeddingTypeNormalizer<S: OntologyStore> {
    store: S,
    embedder: Arc<dyn Embedder>,
    threshold: f32,
    embeddings: Mutex<HashMap<String, Vec<f32>>>,
    decided: Mutex<HashMap<(Kind, String), String>>,
}

impl<S: OntologyStore> EmbeddingTypeNormalizer<S> {
    pub const DEFAULT_THRESHOLD: f32 = 0.85;

    pub fn new(store: S, embedder: Arc<dyn Embedder>, threshold: f32) -> Self {
        Self { store, embedder, threshold, embeddings: Mutex::new(HashMap::new()), decided: Mutex::new(HashMap::new()) }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    async fn embed_all(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ExtractError> {
        let missing: Vec<String> = {
            let cache = self.embeddings.lock().unwrap();
            let mut m: Vec<String> = texts.iter().filter(|t| !cache.contains_key(*t)).cloned().collect();
            m.sort();
            m.dedup();
            m
        };
        if !missing.is_empty() {
            let vecs = self.embedder.embed_texts(&missing).await.map_err(|e| ExtractError::Model(format!("embedding: {e}")))?;
            if vecs.len() != missing.len() {
                return Err(ExtractError::Model(format!("embedding: expected {} vectors, got {}", missing.len(), vecs.len())));
            }
            let mut cache = self.embeddings.lock().unwrap();
            for (t, v) in missing.into_iter().zip(vecs) {
                cache.insert(t, v);
            }
        }
        let cache = self.embeddings.lock().unwrap();
        Ok(texts.iter().map(|t| cache[t].clone()).collect())
    }

    /// Best matching type id, if any clears the threshold.
    async fn best_match(&self, raw: &str, types: &[OntologyType]) -> Result<Option<String>, ExtractError> {
        let raw_slug = slug(raw);
        if let Some(t) = types.iter().find(|t| t.review_status != "rejected" && (t.id == raw_slug || slug(&t.label) == raw_slug || t.synonyms.iter().any(|s| slug(s) == raw_slug))) {
            return Ok(Some(t.id.clone()));
        }
        let candidates: Vec<(usize, String)> = types
            .iter()
            .enumerate()
            .filter(|(_, t)| t.review_status != "rejected")
            .flat_map(|(i, t)| std::iter::once(t.label.clone()).chain(t.synonyms.iter().cloned()).map(move |s| (i, s)))
            .filter(|(_, s)| !s.trim().is_empty())
            .collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let mut texts = vec![raw.trim().to_string()];
        texts.extend(candidates.iter().map(|(_, s)| s.clone()));
        let vecs = self.embed_all(&texts).await?;
        let mut best: Option<(f32, usize)> = None;
        for (k, (i, _)) in candidates.iter().enumerate() {
            let score = cosine(&vecs[0], &vecs[k + 1]);
            if best.map_or(true, |(b, _)| score > b) {
                best = Some((score, *i));
            }
        }
        Ok(best.filter(|(s, _)| *s >= self.threshold).map(|(_, i)| types[i].id.clone()))
    }

    async fn normalise(&self, kind: Kind, raw: &str, roles: &[String]) -> Result<String, ExtractError> {
        let raw = raw.trim();
        if let Some(id) = self.decided.lock().unwrap().get(&(kind, raw.to_lowercase())) {
            return Ok(id.clone());
        }
        let types = match kind {
            Kind::Entity => self.store.entity_types().await?,
            Kind::Relation => self.store.relation_types().await?,
        };
        let id = match self.best_match(raw, &types).await? {
            Some(id) => id,
            None => {
                let id = slug(raw);
                match kind {
                    Kind::Entity => self.store.insert_pending_entity_type(&id, raw).await?,
                    Kind::Relation => self.store.insert_pending_relation_type(&id, raw, roles).await?,
                }
                id
            }
        };
        self.decided.lock().unwrap().insert((kind, raw.to_lowercase()), id.clone());
        Ok(id)
    }
}

#[async_trait]
impl<S: OntologyStore> TypeNormalizer for EmbeddingTypeNormalizer<S> {
    async fn entity_type(&self, raw: &str) -> Result<String, ExtractError> {
        self.normalise(Kind::Entity, raw, &[]).await
    }
    async fn predicate(&self, raw: &str) -> Result<String, ExtractError> {
        self.normalise(Kind::Relation, raw, &[]).await
    }
    async fn predicate_with_roles(&self, raw: &str, roles: &[String]) -> Result<String, ExtractError> {
        self.normalise(Kind::Relation, raw, roles).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vs_models::ModelError;

    /// Deterministic "embedder": a bag of lowercase words hashed into 32 dims.
    pub struct FakeEmbedder {
        pub calls: AtomicUsize,
    }

    #[async_trait]
    impl Embedder for FakeEmbedder {
        fn model_ref(&self) -> String {
            "fake@1".into()
        }
        fn dimensions(&self) -> usize {
            32
        }
        async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0f32; 32];
                    for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                        let h = w.bytes().fold(7usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
                        v[h % 32] += 1.0;
                    }
                    v
                })
                .collect())
        }
    }

    fn ty(id: &str, label: &str, syn: &[&str]) -> OntologyType {
        OntologyType { id: id.into(), label: label.into(), synonyms: syn.iter().map(|s| s.to_string()).collect(), review_status: "approved".into() }
    }

    #[tokio::test]
    async fn matches_exact_similar_and_creates_pending() {
        let store = Arc::new(InMemoryOntologyStore::with_types(
            vec![ty("method", "Method", &["approach", "model architecture"]), ty("dataset", "Dataset", &["benchmark corpus", "corpus"])],
            vec![ty("uses", "uses", &["employs", "applies"])],
        ));
        let emb = Arc::new(FakeEmbedder { calls: AtomicUsize::new(0) });
        let n = EmbeddingTypeNormalizer::new(store.clone(), emb.clone(), 0.85);
        // exact label / synonym slugs need no embedding
        assert_eq!(n.entity_type("Method").await.unwrap(), "method");
        assert_eq!(n.entity_type("Corpus").await.unwrap(), "dataset");
        assert_eq!(emb.calls.load(Ordering::SeqCst), 0);
        // bag-of-words similarity: same words, different order → cosine 1.0 against the synonym
        assert_eq!(n.entity_type("corpus benchmark").await.unwrap(), "dataset");
        assert_eq!(emb.calls.load(Ordering::SeqCst), 1);
        // no match → pending type with slug id
        assert_eq!(n.entity_type("Chemical Compound").await.unwrap(), "chemical_compound");
        let types = store.entity_types().await.unwrap();
        let pending = types.iter().find(|t| t.id == "chemical_compound").unwrap();
        assert_eq!((pending.label.as_str(), pending.review_status.as_str()), ("Chemical Compound", "pending"));
        // decided cache: repeated raw type does not embed again
        let calls = emb.calls.load(Ordering::SeqCst);
        assert_eq!(n.entity_type("chemical compound").await.unwrap(), "chemical_compound");
        assert_eq!(emb.calls.load(Ordering::SeqCst), calls);
        // relations with roles
        assert_eq!(n.predicate_with_roles("employs", &[]).await.unwrap(), "uses");
        assert_eq!(n.predicate_with_roles("outperforms", &["subject".into(), "object".into()]).await.unwrap(), "outperforms");
        let rel = store.relation_types.lock().unwrap();
        let (t, roles) = rel.iter().find(|(t, _)| t.id == "outperforms").unwrap();
        assert_eq!(t.review_status, "pending");
        assert_eq!(roles, &vec!["subject".to_string(), "object".to_string()]);
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[0.0, 1.0]), 0.0);
    }
}
