//! Corpus entity resolution over Postgres + a vector store.
//!
//! Implements [`vs_extract::resolve::CorpusResolver`]: decides whether an entity found in
//! a document is one the corpus already knows, or a new one. The trait itself names no
//! store, which is why this moved here without changing a single caller.
//!
//! The split of work: the **vector store** answers "which entity names are nearest this
//! one", and **Postgres** answers "what are those entities actually called". Similarity
//! is then computed here, from the returned vectors — never from the store's own score,
//! because the thresholds below are only meaningful against a known definition of cosine.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use sqlx::{PgPool, Row};

use vs_core::{HashKey, TenantId};
use vs_extract::resolve::{CorpusResolver, Resolution};
use vs_extract::similarity::{cosine, jaro_winkler, normalise_name};
use vs_extract::{DocEntity, ExtractError};
use vs_models::{ChatRequest, Embedder, OpenAiClient};
use vs_vectors::{EntityHit, VectorStore};

/// LLM judge for candidates in the uncertain score band.
#[derive(Clone)]
pub struct Judge {
    pub client: Arc<OpenAiClient>,
    /// Model id in the registry (a `generic_json` text LLM, task `entity_judge`).
    pub model: String,
}

/// One search hit joined with its stored labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub entity_hk: String,
    pub name: String,
    pub type_id: String,
    pub aliases: Vec<String>,
    /// The sentence this entity was last seen in, as stored by `write_stage2`.
    ///
    /// Empty for an entity written before migration 0008, or one whose mentions were all
    /// in blocks with no text. The judge is shown `(none)` in that case rather than an
    /// empty line, so it can tell a missing context from an uninformative one.
    pub context: String,
    pub cosine: f32,
}

/// What the thresholds say about a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Link,
    Judge,
    New,
}

pub const DEFAULT_LINK_THRESHOLD: f32 = 0.85;
pub const DEFAULT_JUDGE_BAND: (f32, f32) = (0.7, 0.85);
pub const DEFAULT_K: usize = 10;

pub struct PgCorpusResolver {
    pub pool: PgPool,
    pub vectors: Arc<dyn VectorStore>,
    pub embedder: Arc<dyn Embedder>,
    pub judge: Option<Judge>,
    /// Score at or above which a candidate is linked without asking.
    pub link_threshold: f32,
    /// `(low, high)`: scores in `low..high` go to the judge (none configured → new entity).
    pub judge_band: (f32, f32),
    /// Candidates kept after the type filter.
    pub k: usize,
}

impl PgCorpusResolver {
    pub fn new(pool: PgPool, vectors: Arc<dyn VectorStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            pool,
            vectors,
            embedder,
            judge: None,
            link_threshold: DEFAULT_LINK_THRESHOLD,
            judge_band: DEFAULT_JUDGE_BAND,
            k: DEFAULT_K,
        }
    }

    pub fn with_judge(mut self, client: Arc<OpenAiClient>, model: impl Into<String>) -> Self {
        self.judge = Some(Judge { client, model: model.into() });
        self
    }

    pub fn with_thresholds(mut self, link_threshold: f32, judge_band: (f32, f32)) -> Self {
        self.link_threshold = link_threshold;
        self.judge_band = judge_band;
        self
    }

    pub fn with_k(mut self, k: usize) -> Self {
        self.k = k.max(1);
        self
    }

    /// Text embedded for corpus resolution: `name (type)` plus the context sentence.
    ///
    /// Without context this must equal the `entity` embed-target text built by
    /// `vs-graph`, or a search would be comparing against something other than what was
    /// indexed. The test at the bottom of this file is what holds that invariant.
    pub fn embed_text(name: &str, type_id: &str, context: &str) -> String {
        let head = format!("{name} ({type_id})");
        match context.trim() {
            "" => head,
            ctx => format!("{head}\n{ctx}"),
        }
    }

    /// `H(tenant | type_id | normalised name)`: the key of an entity new to the corpus.
    ///
    /// Deterministic on purpose. Two documents processed concurrently that both miss the
    /// vector search still produce the same key, so the second insert hits
    /// `ON CONFLICT DO NOTHING` and they converge on one entity instead of creating a
    /// duplicate pair that nothing will ever merge.
    pub fn new_entity_key(tenant: &TenantId, type_id: &str, name: &str) -> HashKey {
        HashKey::entity(tenant, &format!("{type_id}|{}", normalise_name(name)))
    }

    /// `0.6 · cosine + 0.4 · max Jaro-Winkler(normalised name, candidate name / aliases)`.
    ///
    /// Both halves are needed: cosine alone links "BERT" to "RoBERTa" (similar in
    /// embedding space, different models), and name similarity alone links "Pile" to
    /// "File". The max over the alias cross-product means a match on any known alias
    /// counts as fully as a match on the primary name.
    pub fn score(name: &str, aliases: &[String], candidate: &Candidate) -> f32 {
        let ours: Vec<String> =
            std::iter::once(name).chain(aliases.iter().map(String::as_str)).map(normalise_name).collect();
        let theirs: Vec<String> = std::iter::once(candidate.name.as_str())
            .chain(candidate.aliases.iter().map(String::as_str))
            .map(normalise_name)
            .collect();
        let name_sim = ours.iter().flat_map(|a| theirs.iter().map(move |b| jaro_winkler(a, b))).fold(0.0f32, f32::max);
        0.6 * candidate.cosine.clamp(-1.0, 1.0) + 0.4 * name_sim
    }

    pub fn verdict(&self, score: f32) -> Verdict {
        if score >= self.link_threshold {
            Verdict::Link
        } else if score >= self.judge_band.0 && score < self.judge_band.1 {
            Verdict::Judge
        } else {
            Verdict::New
        }
    }

    /// Same type id, or either side has none.
    ///
    /// "Either side has none" is why this cannot be a server-side filter: an entity whose
    /// type never normalised is compatible with everything, which no index predicate can
    /// express.
    pub fn compatible_type(ours: Option<&str>, theirs: &str) -> bool {
        match ours {
            None | Some("") => true,
            Some(t) => theirs.is_empty() || t == theirs,
        }
    }

    /// Loads names, types and aliases for the hits, newest satellite row per entity.
    ///
    /// Tenant-scoped in the same query as a second line of defence: the vector store
    /// already filters by tenant, but this is the system of record and should not depend
    /// on another store having got it right.
    async fn load_labels(&self, tenant: &TenantId, hits: &[EntityHit]) -> Result<Vec<Candidate>, ExtractError> {
        if hits.is_empty() {
            return Ok(vec![]);
        }
        let keys: Vec<String> = hits.iter().map(|h| h.entity_hk.clone()).collect();
        let rows = sqlx::query(
            "SELECT h.entity_hk, l.name, COALESCE(l.type_id, '') AS type_id, l.aliases, l.context
             FROM vault.hub_entity h
             JOIN LATERAL (
                 SELECT s.name, s.type_id, s.aliases, s.context FROM vault.sat_entity_label s
                 WHERE s.entity_hk = h.entity_hk ORDER BY s.load_dts DESC LIMIT 1
             ) l ON true
             WHERE h.entity_hk = ANY($1) AND h.tenant_id = $2",
        )
        .bind(&keys)
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ExtractError::Store(format!("postgres: {e}")))?;

        let mut by_hk: std::collections::HashMap<String, Candidate> = std::collections::HashMap::new();
        for r in &rows {
            let entity_hk: String = r.get("entity_hk");
            let aliases: serde_json::Value = r.get("aliases");
            by_hk.insert(
                entity_hk.clone(),
                Candidate {
                    entity_hk,
                    name: r.get("name"),
                    type_id: r.get("type_id"),
                    aliases: aliases
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    context: r.get("context"),
                    cosine: 0.0,
                },
            );
        }
        // Keep the search's order, drop hits with no row: a vector whose entity is not in
        // this tenant's hub is either another tenant's or a leftover, and must not be
        // offered as a candidate.
        let mut seen = HashSet::new();
        Ok(hits
            .iter()
            .filter(|h| seen.insert(h.entity_hk.clone()))
            .filter_map(|h| by_hk.get(&h.entity_hk).cloned())
            .collect())
    }

    /// The decision for one entity, given the hits and the query vector. Separated from
    /// the store calls so it can be unit-tested without Postgres or Qdrant.
    pub async fn decide(
        &self,
        tenant: &TenantId,
        entity: &DocEntity,
        context: &str,
        query: &[f32],
        mut found: Vec<Candidate>,
        vectors_by_hk: &std::collections::HashMap<String, Vec<f32>>,
    ) -> Result<Resolution, ExtractError> {
        let type_id = entity.type_id.as_deref().unwrap_or(&entity.raw_type);
        found.retain(|c| Self::compatible_type(entity.type_id.as_deref(), &c.type_id));

        // Candidates whose stored vector is missing or of another dimension (a different
        // embedding model wrote it) are re-embedded from `name (type)` so they still get
        // a real cosine rather than silently scoring 0.
        let mut missing = Vec::new();
        for (i, c) in found.iter_mut().enumerate() {
            match vectors_by_hk.get(&c.entity_hk) {
                Some(v) if v.len() == query.len() => c.cosine = cosine(query, v),
                _ => missing.push(i),
            }
        }
        if !missing.is_empty() {
            let texts: Vec<String> =
                missing.iter().map(|&i| Self::embed_text(&found[i].name, &found[i].type_id, "")).collect();
            let vecs = self.embedder.embed_texts(&texts).await.map_err(|e| ExtractError::Model(e.to_string()))?;
            for (&i, v) in missing.iter().zip(&vecs) {
                found[i].cosine = cosine(query, v);
            }
        }

        let mut scored: Vec<(f32, Candidate)> =
            found.into_iter().map(|c| (Self::score(&entity.name, &entity.aliases, &c), c)).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(self.k);

        for (score, cand) in &scored {
            match self.verdict(*score) {
                Verdict::Link => return Ok(Resolution::Link { entity_hk: stored_key(&cand.entity_hk)?, score: *score }),
                Verdict::Judge => {
                    let Some(judge) = &self.judge else { break };
                    if judge.same_entity(entity, type_id, context, cand).await? {
                        return Ok(Resolution::Link { entity_hk: stored_key(&cand.entity_hk)?, score: *score });
                    }
                }
                // The list is sorted, so nothing below this can score better.
                Verdict::New => break,
            }
        }
        Ok(Resolution::New { entity_hk: Self::new_entity_key(tenant, type_id, &entity.name) })
    }
}

#[async_trait]
impl CorpusResolver for PgCorpusResolver {
    async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, context: &str) -> Result<Resolution, ExtractError> {
        let type_id = entity.type_id.as_deref().unwrap_or(&entity.raw_type);
        let text = Self::embed_text(&entity.name, type_id, context);
        let query = self
            .embedder
            .embed_texts(std::slice::from_ref(&text))
            .await
            .map_err(|e| ExtractError::Model(e.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| ExtractError::Model("embedder returned no vector".into()))?;

        // The vector store filters by tenant before the search, so `k` candidates means
        // `k` of this tenant's candidates — no over-fetching needed.
        let hits = self
            .vectors
            .search_entities(tenant, &query, self.k)
            .await
            .map_err(|e| ExtractError::Store(e.to_string()))?;
        let vectors_by_hk: std::collections::HashMap<String, Vec<f32>> =
            hits.iter().filter_map(|h| h.vector.clone().map(|v| (h.entity_hk.clone(), v))).collect();
        let found = self.load_labels(tenant, &hits).await?;
        self.decide(tenant, entity, context, &query, found, &vectors_by_hk).await
    }
}

impl Judge {
    /// Asks the LLM whether `entity` (with its context sentence) and `cand` are the same
    /// real-world entity; constrained to `{"same": bool}`.
    async fn same_entity(
        &self,
        entity: &DocEntity,
        type_id: &str,
        context: &str,
        cand: &Candidate,
    ) -> Result<bool, ExtractError> {
        let prompt = format!(
            // Both sides carry a context sentence. Asking this question with the
            // evidence on one side only — which is what the previous store could manage —
            // reduced it to comparing two names, and two identical names are not a
            // question a model can answer either way.
            "Entity A (from the document being processed)\n  name: {}\n  type: {}\n  aliases: {}\n  context: {}\n\n\
             Entity B (already in the corpus)\n  name: {}\n  type: {}\n  aliases: {}\n  context: {}\n\n\
             Are A and B the same real-world entity (same method, dataset, organisation, …), not merely related or similar?",
            entity.name,
            type_id,
            join_or_none(&entity.aliases),
            or_none(context),
            cand.name,
            if cand.type_id.is_empty() { "(unknown)" } else { &cand.type_id },
            join_or_none(&cand.aliases),
            or_none(&cand.context),
        );
        let req = ChatRequest {
            system: Some(
                "You decide whether two entity records refer to the same real-world entity. \
                 Answer only with the JSON object."
                    .into(),
            ),
            prompt,
            images: vec![],
            max_tokens: 32,
            // Corpus resolution runs in Stage 2, after every page: batch work, never
            // competing with an interactive page for a reserved replica.
            interactive: false,
            json_schema: Some(json!({
                "type": "object",
                "properties": { "same": { "type": "boolean" } },
                "required": ["same"],
                "additionalProperties": false
            })),
        };
        let response = self.client.chat(&self.model, &req).await.map_err(|e| ExtractError::Model(e.to_string()))?;
        let parsed: serde_json::Value = serde_json::from_str(response.text.trim())
            .map_err(|e| ExtractError::Schema(format!("judge returned {:?}: {e}", response.text)))?;
        parsed["same"].as_bool().ok_or_else(|| ExtractError::Schema(format!("judge returned {parsed}")))
    }
}

/// A hash key as the store holds it (`HashKey` is serde-transparent over its hex string).
fn stored_key(hex: &str) -> Result<HashKey, ExtractError> {
    crate::hash_key_from_hex(hex).ok_or_else(|| ExtractError::Schema(format!("malformed entity_hk {hex:?}")))
}

fn join_or_none(xs: &[String]) -> String {
    if xs.is_empty() { "(none)".to_string() } else { xs.join(", ") }
}

/// `(none)` for an absent context, so the judge can tell "no sentence was stored" from
/// "the sentence said nothing".
fn or_none(s: &str) -> &str {
    if s.trim().is_empty() { "(none)" } else { s.trim() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cand(name: &str, type_id: &str, aliases: &[&str], cosine: f32) -> Candidate {
        Candidate {
            entity_hk: "0".repeat(64),
            name: name.into(),
            type_id: type_id.into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            context: String::new(),
            cosine,
        }
    }

    #[test]
    fn score_mixes_cosine_and_best_name_similarity() {
        let exact = PgCorpusResolver::score("BERT", &[], &cand("BERT", "method", &[], 1.0));
        assert!((exact - 1.0).abs() < 1e-5, "identical name and vector must score 1.0, got {exact}");
        // A match on an alias counts as fully as a match on the primary name.
        let via_alias = PgCorpusResolver::score("BERT", &[], &cand("Bidirectional Encoder", "method", &["BERT"], 1.0));
        assert!((via_alias - 1.0).abs() < 1e-5, "alias match should score like a name match, got {via_alias}");
        // An unrelated name stays below the link threshold even at a high cosine.
        assert!(PgCorpusResolver::score("BERT", &[], &cand("ImageNet", "method", &[], 0.95)) < 0.85);

        // Known sensitivity, asserted so a threshold change has to confront it: at
        // cosine 0.95 a *similar* wrong name still links. "RoBERTa" scores 0.8795,
        // over the 0.85 default. The 0.4 name weight is not enough on its own to
        // separate model families, so `link_threshold` is the real guard — which is
        // why it is configured to 0.92 in production, not the 0.85 default here.
        let near_miss = PgCorpusResolver::score("BERT", &[], &cand("RoBERTa", "method", &[], 0.95));
        assert!((0.87..0.89).contains(&near_miss), "scoring drifted: {near_miss}");
        assert!(near_miss < 0.92, "must stay below the production link threshold");
    }

    #[test]
    fn type_compatibility() {
        assert!(PgCorpusResolver::compatible_type(None, "method"));
        assert!(PgCorpusResolver::compatible_type(Some(""), "method"));
        assert!(PgCorpusResolver::compatible_type(Some("method"), ""));
        assert!(PgCorpusResolver::compatible_type(Some("method"), "method"));
        assert!(!PgCorpusResolver::compatible_type(Some("method"), "dataset"));
    }

    struct TestEmbedder;

    #[async_trait]
    impl Embedder for TestEmbedder {
        fn model_ref(&self) -> String {
            "test@1".into()
        }
        fn dimensions(&self) -> usize {
            4
        }
        async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, vs_models::ModelError> {
            Ok(texts.iter().map(|_| vec![0.0, 0.0, 0.0, 1.0]).collect())
        }
    }

    fn resolver() -> PgCorpusResolver {
        // The pool is never touched: `decide` does no I/O of its own.
        let pool = PgPool::connect_lazy("postgres://vs:vs@127.0.0.1/visual_suite").unwrap();
        struct NoVectors;
        #[async_trait]
        impl VectorStore for NoVectors {
            async fn ensure_ready(&self, _: usize) -> Result<(), vs_vectors::VectorError> {
                Ok(())
            }
            async fn write_run(
                &self,
                _: &vs_graph::RunInfo,
                _: &vs_graph::DocGraph,
                _: &vs_graph::Vectors,
            ) -> Result<(), vs_vectors::VectorError> {
                Ok(())
            }
            async fn drop_run(&self, _: &TenantId, _: &str, _: &str) -> Result<(), vs_vectors::VectorError> {
                Ok(())
            }
            async fn search_entities(
                &self,
                _: &TenantId,
                _: &[f32],
                _: usize,
            ) -> Result<Vec<EntityHit>, vs_vectors::VectorError> {
                Ok(vec![])
            }
            async fn search(
                &self,
                _: &TenantId,
                _: vs_vectors::Scope,
                _: &vs_vectors::HybridQuery,
            ) -> Result<Vec<vs_vectors::Hit>, vs_vectors::VectorError> {
                Ok(vec![])
            }
            async fn migrate_dimensions(&self, _: usize) -> Result<Vec<String>, vs_vectors::VectorError> {
                Ok(vec![])
            }
        }
        PgCorpusResolver::new(pool, Arc::new(NoVectors), Arc::new(TestEmbedder))
    }

    fn entity(name: &str, type_id: Option<&str>) -> DocEntity {
        DocEntity {
            local_id: "e1".into(),
            name: name.into(),
            raw_type: "method".into(),
            type_id: type_id.map(String::from),
            aliases: vec![],
            mentions: vec![],
            entity_hk: None,
            context: String::new(),
        }
    }

    #[tokio::test]
    async fn verdict_thresholds() {
        let r = resolver();
        assert_eq!(r.verdict(0.9), Verdict::Link);
        assert_eq!(r.verdict(0.85), Verdict::Link);
        assert_eq!(r.verdict(0.8), Verdict::Judge);
        assert_eq!(r.verdict(0.7), Verdict::Judge);
        assert_eq!(r.verdict(0.69), Verdict::New);
    }

    #[tokio::test]
    async fn links_on_a_strong_match_and_creates_otherwise() {
        let r = resolver();
        let tenant = TenantId("t".into());
        let query = vec![0.0, 0.0, 0.0, 1.0];
        let hk = "a".repeat(64);

        let mut vectors = HashMap::new();
        vectors.insert(hk.clone(), query.clone());
        let mut c = cand("BERT", "method", &[], 0.0);
        c.entity_hk = hk.clone();

        let linked = r
            .decide(&tenant, &entity("BERT", Some("method")), "", &query, vec![c], &vectors)
            .await
            .unwrap();
        match linked {
            Resolution::Link { entity_hk, score } => {
                assert_eq!(entity_hk.as_str(), hk);
                assert!(score > 0.99, "score was {score}");
            }
            other => panic!("expected a link, got {other:?}"),
        }

        // No candidates at all → a new, deterministic key.
        let created = r.decide(&tenant, &entity("BERT", Some("method")), "", &query, vec![], &HashMap::new()).await.unwrap();
        let expected = PgCorpusResolver::new_entity_key(&tenant, "method", "BERT");
        assert_eq!(created, Resolution::New { entity_hk: expected });
    }

    /// Without a judge configured, a mid-band score must create rather than link.
    #[tokio::test]
    async fn judge_band_without_a_judge_creates() {
        let r = resolver();
        let tenant = TenantId("t".into());
        let query = vec![0.0, 0.0, 0.0, 1.0];
        let hk = "b".repeat(64);
        let mut vectors = HashMap::new();
        // Unit vector at cosine 0.8 to the query, so the score lands in the judge band:
        // 0.6 * 0.8 + 0.4 * jw("bert", "roberta") = 0.48 + 0.31 = 0.79.
        vectors.insert(hk.clone(), vec![0.6, 0.0, 0.0, 0.8]);
        let mut c = cand("RoBERTa", "method", &[], 0.0);
        c.entity_hk = hk;
        let out = r.decide(&tenant, &entity("BERT", Some("method")), "", &query, vec![c], &vectors).await.unwrap();
        assert!(matches!(out, Resolution::New { .. }), "got {out:?}");
    }

    /// The deterministic key is what makes two concurrent documents converge.
    #[test]
    fn new_entity_key_is_stable_across_surface_forms() {
        let t = TenantId("t".into());
        let a = PgCorpusResolver::new_entity_key(&t, "method", "  Vision   Transformer ");
        let b = PgCorpusResolver::new_entity_key(&t, "method", "vision transformer");
        assert_eq!(a.as_str(), b.as_str());
        assert_ne!(a.as_str(), PgCorpusResolver::new_entity_key(&t, "dataset", "vision transformer").as_str());
    }

    /// The search is only meaningful if the query text matches what was indexed.
    #[test]
    fn embed_text_matches_the_graph_entity_target() {
        let name = "BERT";
        let type_id = "method";
        // This is verbatim what vs-graph builds for an entity embed target.
        let graph_target = format!("{} ({})", name, type_id);
        assert_eq!(PgCorpusResolver::embed_text(name, type_id, ""), graph_target);
        assert_eq!(PgCorpusResolver::embed_text(name, type_id, "   "), graph_target);
        assert_eq!(PgCorpusResolver::embed_text(name, type_id, "ctx"), format!("{graph_target}\nctx"));
    }
}
