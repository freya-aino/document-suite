//! Role `doc` (Stage 2). See SPEC.md "Role doc".
//!
//! `stage2` is the pure pipeline over trait objects (extractor, corpus resolver,
//! embedder) so it can be tested with fakes; `DocHandler` wraps it with the job row,
//! blob, vault and Helix writes. Every step is idempotent on retry: page results are
//! read-only, the vault writer upserts, `GraphStore::write_run` drops a partial earlier
//! attempt of the same run first, and `doc.json` is overwritten.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use vs_core::{DocType, HashKey, JobSpec, JobState, PageResult, RunId, Task, TenantId};
use vs_extract::{CorpusResolver, DocProfile, EmbeddingTypeNormalizer, ExtractConfig, ExtractError, ExtractionOutput, Extractor, LlmExtractor};
use vs_graph::{DocGraph, EmbedTarget};
use vs_helix::{GraphStore, RunInfo, Vectors};
use vs_infra::blob::{keys, BlobError, BlobStore};
use vs_infra::config::Stage2Config;
use vs_models::{Embedder, ModelError, OpenAiClient};
use vs_stitch::{StitchError, StitchedDocument};
use vs_vault::ops::Ops;
use vs_vault::{PgOntologyStore, RunMeta, VaultWriter};

use crate::runtime::{Handler, TaskError};

/// Concurrent corpus-resolution calls per document.
const RESOLVE_CONCURRENCY: usize = 8;
const DOC_RESULT_SCHEMA_VERSION: u32 = 1;

pub struct DocHandler {
    pub blob: Arc<dyn BlobStore>,
    pub vault: Arc<dyn VaultWriter>,
    pub ops: Arc<Ops>,
    pub graph: Arc<dyn GraphStore>,
    pub client: Arc<OpenAiClient>,
    pub ontology: Arc<PgOntologyStore>,
    pub embedder: Arc<dyn Embedder>,
    pub resolver: Arc<dyn CorpusResolver>,
    pub stage2: Stage2Config,
    pub max_failed_page_fraction: f32,
}

#[async_trait]
impl Handler for DocHandler {
    async fn handle(&self, task: &Task) -> Result<(), TaskError> {
        let Task::Doc { job_id, run_id, document_hk, spec } = task else {
            return Err(TaskError::permanent(anyhow::anyhow!("doc worker received {task:?}")));
        };
        self.process(*job_id, *run_id, document_hk, spec).await
    }

    async fn give_up(&self, task: &Task, error: &str) {
        let job_id = crate::runtime::job_id(task);
        if let Err(e) = self.ops.set_state(job_id, JobState::Failed, Some(error)).await {
            tracing::error!(%job_id, error = %e, "could not mark job failed");
        }
    }
}

impl DocHandler {
    async fn process(&self, job_id: uuid::Uuid, run_id: RunId, document_hk: &HashKey, spec: &JobSpec) -> Result<(), TaskError> {
        let job = self.ops.get_job(&spec.tenant, job_id).await?.ok_or_else(|| TaskError::permanent(anyhow::anyhow!("job {job_id} not found")))?;
        let pages_total =
            job.pages_total.filter(|n| *n > 0).ok_or_else(|| TaskError::permanent(anyhow::anyhow!("job {job_id} has no pages")))? as u32;
        self.ops.set_state(job_id, JobState::Stitching, None).await?;

        let pages = load_pages(self.blob.clone(), document_hk.as_str().to_string(), run_id.to_string(), pages_total).await?;
        let pages_failed = pages_total - pages.len() as u32;
        if too_many_failed(pages_failed, pages_total, self.max_failed_page_fraction) {
            return Err(TaskError::permanent(anyhow::anyhow!(
                "{pages_failed} of {pages_total} pages failed (limit {:.0}%)",
                self.max_failed_page_fraction * 100.0
            )));
        }
        let doc_profile = dominant_doc_type(&pages).map(doc_profile_for).unwrap_or_default();
        let doc_types: BTreeSet<String> = pages.iter().map(|p| vs_vault::enum_name(&p.profile.doc_type)).collect();
        let stitched = vs_stitch::stitch(pages, pages_total).map_err(stitch_error)?;
        tracing::info!(blocks = stitched.blocks.len(), sections = stitched.sections.len(), pages_failed, ?doc_profile, "stitched");

        self.ops.set_state(job_id, JobState::DocProcessing, None).await?;
        let mut ext_cfg = ExtractConfig::new(&self.stage2.extraction_model, &self.stage2.verify_model);
        ext_cfg.window_chars = self.stage2.window_chars;
        ext_cfg.type_match_threshold = self.stage2.type_match_threshold;
        ext_cfg.language = Some(spec.language.as_str().to_string());
        ext_cfg.doc_profile = doc_profile;
        let normalizer = EmbeddingTypeNormalizer::new(self.ontology.clone(), self.embedder.clone(), self.stage2.type_match_threshold);
        let extractor = LlmExtractor::new(self.client.clone(), ext_cfg, Arc::new(normalizer));
        let run_info = RunInfo {
            tenant: spec.tenant.clone(),
            run_id,
            language: spec.language.as_str().to_string(),
            page_count: pages_total,
            doc_types: doc_types.into_iter().collect(),
            new_entities: HashSet::new(),
        };
        let out = stage2(&stitched, spec, run_info, &extractor, self.resolver.as_ref(), self.embedder.as_ref(), self.embed_batch()).await?;

        self.ops.set_state(job_id, JobState::Writing, None).await?;
        let final_state = if pages_failed > 0 { JobState::Partial } else { JobState::Done };
        let dag_list_version = match &spec.dag_list_version {
            Some(v) => v.clone(),
            None => self.ops.active_dag_list_version(&spec.tenant).await?.unwrap_or_else(|| "none".into()),
        };
        let run_meta = RunMeta { run_id, dag_list_version, started_at: job.created_at };
        // Vault first: it is the system of record; a Helix failure retries the task.
        self.vault.write_stage2(&run_meta, spec, &stitched, &out.ext, final_state).await?;
        self.graph.write_run(&out.run_info, &out.graph, &out.vectors).await?;

        let result = doc_result(&stitched, &out, pages_total, pages_failed, final_state);
        let body = serde_json::to_vec(&result)?;
        self.blob.put(&keys::doc_result(document_hk.as_str(), &run_id.to_string()), body.into(), "application/json").await?;
        self.ops.set_state(job_id, final_state, None).await?;
        tracing::info!(%job_id, entities = out.ext.entities.len(), assertions = out.ext.assertions.len(), state = ?final_state, "document done");
        Ok(())
    }

    fn embed_batch(&self) -> usize {
        if self.stage2.embed_batch_size == 0 {
            32
        } else {
            self.stage2.embed_batch_size
        }
    }
}

/// Page results of the run; a missing object is a failed page. Owned arguments: a
/// borrowing async fn here trips rustc's "Send is not general enough" on the handler.
async fn load_pages(blob: Arc<dyn BlobStore>, document_hk: String, run: String, pages_total: u32) -> Result<Vec<PageResult>, TaskError> {
    let mut pages = Vec::with_capacity(pages_total as usize);
    let mut missing = Vec::new();
    for n in 1..=pages_total {
        let fetched = blob.get(&keys::page_result(&document_hk, &run, n)).await;
        match fetched {
            Ok(bytes) => pages.push(parse_page(&bytes, n)?),
            Err(BlobError::NotFound(_)) => missing.push(n),
            Err(e) => return Err(TaskError::from(e)),
        }
    }
    if !missing.is_empty() {
        tracing::warn!(?missing, "pages without a result count as failed");
    }
    Ok(pages)
}

fn parse_page(bytes: &[u8], page_no: u32) -> Result<PageResult, TaskError> {
    serde_json::from_slice(bytes).map_err(|e| TaskError::permanent(anyhow::anyhow!("page result {page_no}: {e}")))
}

pub struct Stage2Output {
    pub ext: ExtractionOutput,
    pub graph: DocGraph,
    pub vectors: Vectors,
    pub run_info: RunInfo,
}

/// Extraction → corpus resolution → graph → vectors. Pure over its trait inputs.
pub async fn stage2(
    stitched: &StitchedDocument,
    spec: &JobSpec,
    mut run_info: RunInfo,
    extractor: &dyn Extractor,
    resolver: &dyn CorpusResolver,
    embedder: &dyn Embedder,
    embed_batch: usize,
) -> Result<Stage2Output, TaskError> {
    let mut ext = extractor.extract(stitched, spec.profile).await.map_err(extract_error)?;
    resolve_entities(&mut ext, stitched, &spec.tenant, resolver, &mut run_info).await?;

    let title = stitched.blocks.iter().find(|b| b.block.kind == vs_core::BlockKind::Title).map(|b| b.block.text.clone());
    let graph = vs_graph::build(stitched, &ext, title.as_deref());
    graph.validate().map_err(|e| TaskError::permanent(anyhow::anyhow!("graph invariant violated: {e:?}")))?;

    let vectors = embed_targets(&graph.embed_targets, embedder, embed_batch).await?;
    Ok(Stage2Output { ext, graph, vectors, run_info })
}

/// Sets `entity_hk` on every document entity from the corpus resolver, using the text
/// of the block of the entity's first mention as context.
async fn resolve_entities(
    ext: &mut ExtractionOutput,
    stitched: &StitchedDocument,
    tenant: &TenantId,
    resolver: &dyn CorpusResolver,
    run_info: &mut RunInfo,
) -> Result<(), TaskError> {
    let mention_block: HashMap<&str, &HashKey> = ext.mentions.iter().map(|m| (m.id.as_str(), &m.block_hk)).collect();
    let contexts: Vec<String> = ext
        .entities
        .iter()
        .map(|e| {
            e.mentions
                .iter()
                .filter_map(|m| mention_block.get(m.as_str()))
                .filter_map(|hk| stitched.index_of(hk))
                .map(|i| stitched.blocks[i].block.text.clone())
                .next()
                .unwrap_or_default()
        })
        .collect();
    let limit = Arc::new(tokio::sync::Semaphore::new(RESOLVE_CONCURRENCY));
    let futs = ext.entities.iter().zip(contexts).enumerate().map(|(i, (e, ctx))| {
        let limit = limit.clone();
        async move {
            let _slot = limit.acquire().await.expect("semaphore is never closed");
            resolver.resolve(tenant, e, &ctx).await.map(|r| (i, r))
        }
    });
    let resolved: Vec<(usize, vs_extract::Resolution)> = futures::future::try_join_all(futs).await.map_err(extract_error)?;
    for (i, r) in resolved {
        run_info.note_resolution(&r);
        ext.entities[i].entity_hk = Some(match r {
            vs_extract::Resolution::Link { entity_hk, .. } | vs_extract::Resolution::New { entity_hk } => entity_hk,
        });
    }
    Ok(())
}

/// Embeds every target's text in batches of `batch` and keys the vectors by node.
pub async fn embed_targets(targets: &[EmbedTarget], embedder: &dyn Embedder, batch: usize) -> Result<Vectors, TaskError> {
    let mut by_key = HashMap::with_capacity(targets.len());
    for chunk in targets.chunks(batch.max(1)) {
        let texts: Vec<String> = chunk.iter().map(|t| t.text.clone()).collect();
        let vecs = embedder.embed_texts(&texts).await.map_err(model_error)?;
        if vecs.len() != chunk.len() {
            return Err(anyhow::anyhow!("embedder returned {} vectors for {} texts", vecs.len(), chunk.len()).into());
        }
        by_key.extend(chunk.iter().map(|t| t.node_key.clone()).zip(vecs));
    }
    Ok(Vectors { model_ref: embedder.model_ref(), dimensions: embedder.dimensions(), by_key })
}

pub fn too_many_failed(failed: u32, total: u32, max_fraction: f32) -> bool {
    total == 0 || failed as f32 / total as f32 > max_fraction
}

/// Most frequent page doc type; ties go to the type seen first.
pub fn dominant_doc_type(pages: &[PageResult]) -> Option<DocType> {
    let mut counts: Vec<(DocType, usize)> = Vec::new();
    for p in pages {
        match counts.iter_mut().find(|(t, _)| *t == p.profile.doc_type) {
            Some((_, n)) => *n += 1,
            None => counts.push((p.profile.doc_type, 1)),
        }
    }
    // Strictly greater keeps the earliest type on ties.
    counts
        .into_iter()
        .fold(None, |best: Option<(DocType, usize)>, c| match best {
            Some(b) if b.1 >= c.1 => Some(b),
            _ => Some(c),
        })
        .map(|(t, _)| t)
}

pub fn doc_profile_for(t: DocType) -> DocProfile {
    match t {
        DocType::ScientificPaper => DocProfile::Scientific,
        DocType::Contract => DocProfile::Legal,
        DocType::Invoice | DocType::Receipt => DocProfile::Financial,
        _ => DocProfile::Generic,
    }
}

/// `doc.json`: the stitched document plus a summary of what Stage 2 produced.
#[derive(Debug, Serialize)]
pub struct DocResult<'a> {
    pub schema_version: u32,
    pub document_hk: &'a HashKey,
    pub run_id: RunId,
    pub state: JobState,
    pub pages_total: u32,
    pub pages_failed: u32,
    pub missing_pages: &'a [u32],
    pub stitched: &'a StitchedDocument,
    pub extraction: ExtractionSummary,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ExtractionSummary {
    pub mentions: usize,
    pub entities: usize,
    pub new_entities: usize,
    pub linked_entities: usize,
    pub assertions: usize,
    pub verified_assertions: usize,
    pub dropped_ungrounded: u32,
    pub dropped_unverified: u32,
    pub graph_nodes: usize,
    pub graph_edges: usize,
    pub vectors: usize,
}

pub fn doc_result<'a>(stitched: &'a StitchedDocument, out: &'a Stage2Output, pages_total: u32, pages_failed: u32, state: JobState) -> DocResult<'a> {
    let ext = &out.ext;
    let new_entities = ext.entities.iter().filter(|e| e.entity_hk.as_ref().is_some_and(|hk| out.run_info.new_entities.contains(hk.as_str()))).count();
    DocResult {
        schema_version: DOC_RESULT_SCHEMA_VERSION,
        document_hk: &stitched.document_hk,
        run_id: stitched.run_id,
        state,
        pages_total,
        pages_failed,
        missing_pages: &stitched.missing_pages,
        stitched,
        extraction: ExtractionSummary {
            mentions: ext.mentions.len(),
            entities: ext.entities.len(),
            new_entities,
            linked_entities: ext.entities.len() - new_entities,
            assertions: ext.assertions.len(),
            verified_assertions: ext.assertions.iter().filter(|a| a.verified).count(),
            dropped_ungrounded: ext.dropped_ungrounded,
            dropped_unverified: ext.dropped_unverified,
            graph_nodes: out.graph.nodes.len(),
            graph_edges: out.graph.edges.len(),
            vectors: out.vectors.by_key.len(),
        },
    }
}

fn stitch_error(e: StitchError) -> TaskError {
    TaskError::permanent(e) // Empty / Mixed are data problems
}

/// Model and store outages retry; schema violations of the extraction are final.
fn extract_error(e: ExtractError) -> TaskError {
    match e {
        ExtractError::Model(_) | ExtractError::Store(_) => TaskError::from(e),
        ExtractError::Schema(_) | ExtractError::NotImplemented(_) => TaskError::permanent(e),
    }
}

fn model_error(e: ModelError) -> TaskError {
    match e {
        ModelError::UnknownModel(_) | ModelError::Config(_) => TaskError::permanent(e),
        _ => TaskError::from(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::{BBox, Block, BlockContent, BlockKind, ClassifierTier, LanguageTag, PageProfile, Profile, Provenance};
    use vs_extract::{DocEntity, Mention, Resolution};

    fn profile(doc_type: DocType) -> PageProfile {
        PageProfile {
            language: LanguageTag::parse("en").unwrap(),
            doc_type,
            born_digital: true,
            text_layer_coverage: 1.0,
            has_handwriting: false,
            has_images: false,
            has_tables: false,
            has_formulas: false,
            has_charts: false,
            rotation_deg: 0,
            confidence: 0.9,
            tier: ClassifierTier::Rules,
        }
    }

    fn block(page_hk: &HashKey, run: &RunId, index: u32, kind: BlockKind, text: &str) -> Block {
        Block {
            block_hk: HashKey::block(page_hk, run, index),
            index,
            kind,
            bbox: BBox::new(0.0, 0.1 * index as f32, 1.0, 0.1 * index as f32 + 0.05).unwrap(),
            reading_order: index,
            content: BlockContent::Text { text: text.into() },
            text: text.into(),
            level: None,
            confidence: 0.9,
            provenance: Provenance {
                run_id: *run,
                dag_ref: "d@1".into(),
                node_id: "n".into(),
                step_kind: "s".into(),
                model_ref: None,
                prompt_ref: None,
                params_hash: "0".into(),
                started_at: chrono::Utc::now(),
                duration_ms: 1,
            },
        }
    }

    fn page(doc: &HashKey, run: RunId, page_no: u32, doc_type: DocType) -> PageResult {
        let page_hk = HashKey::page(doc, page_no);
        PageResult {
            schema_version: 1,
            document_hk: doc.clone(),
            page_hk: page_hk.clone(),
            page_no,
            run_id: run,
            render_width_px: 100,
            render_height_px: 100,
            profile: profile(doc_type),
            dag_ref: "d@1".into(),
            blocks: vec![block(&page_hk, &run, 0, BlockKind::Title, "A Title"), block(&page_hk, &run, 1, BlockKind::Paragraph, "BERT is a model.")],
        }
    }

    fn spec() -> JobSpec {
        serde_json::from_value(serde_json::json!({ "tenant": "t", "language": "en", "source_uri": "k" })).unwrap()
    }

    #[test]
    fn failed_fraction() {
        assert!(!too_many_failed(0, 10, 0.05));
        assert!(!too_many_failed(1, 20, 0.05)); // exactly at the limit passes
        assert!(too_many_failed(2, 20, 0.05));
        assert!(too_many_failed(0, 0, 0.05), "no pages at all");
    }

    #[test]
    fn dominant_type_and_profile() {
        let doc = HashKey::of(&["d"]);
        let run = RunId::new();
        assert_eq!(dominant_doc_type(&[]), None);
        let pages = vec![page(&doc, run, 1, DocType::Report), page(&doc, run, 2, DocType::Invoice), page(&doc, run, 3, DocType::Invoice)];
        assert_eq!(dominant_doc_type(&pages), Some(DocType::Invoice));
        let tie = vec![page(&doc, run, 1, DocType::Contract), page(&doc, run, 2, DocType::Report)];
        assert_eq!(dominant_doc_type(&tie), Some(DocType::Contract), "tie goes to the first seen");
        assert_eq!(doc_profile_for(DocType::ScientificPaper), DocProfile::Scientific);
        assert_eq!(doc_profile_for(DocType::Contract), DocProfile::Legal);
        assert_eq!(doc_profile_for(DocType::Receipt), DocProfile::Financial);
        assert_eq!(doc_profile_for(DocType::Slide), DocProfile::Generic);
    }

    struct FakeEmbedder;

    #[async_trait]
    impl Embedder for FakeEmbedder {
        fn model_ref(&self) -> String {
            "embed@1".into()
        }
        fn dimensions(&self) -> usize {
            2
        }
        async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
            assert!(texts.len() <= 2, "batches of at most 2");
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
        }
    }

    #[tokio::test]
    async fn vectors_are_keyed_by_node_and_batched() {
        let targets: Vec<EmbedTarget> =
            (0..5).map(|i| EmbedTarget { node_key: format!("k{i}"), kind: vs_graph::NodeKind::Block, text: "x".repeat(i + 1) }).collect();
        let v = embed_targets(&targets, &FakeEmbedder, 2).await.unwrap();
        assert_eq!((v.model_ref.as_str(), v.dimensions, v.by_key.len()), ("embed@1", 2, 5));
        assert_eq!(v.by_key["k4"], vec![5.0, 1.0]);
        assert!(embed_targets(&[], &FakeEmbedder, 2).await.unwrap().by_key.is_empty());
    }

    /// One entity "BERT" mentioned in the paragraph; no assertions.
    struct FakeExtractor;

    #[async_trait]
    impl Extractor for FakeExtractor {
        async fn extract(&self, doc: &StitchedDocument, _: Profile) -> Result<ExtractionOutput, ExtractError> {
            let para = doc.blocks.iter().find(|b| b.block.kind == BlockKind::Paragraph).unwrap();
            Ok(ExtractionOutput {
                mentions: vec![Mention {
                    id: "m1".into(),
                    block_hk: para.block.block_hk.clone(),
                    char_start: 0,
                    char_end: 4,
                    surface: "BERT".into(),
                    raw_type: "model".into(),
                    confidence: 0.9,
                }],
                entities: vec![
                    DocEntity {
                        local_id: "e1".into(),
                        name: "BERT".into(),
                        raw_type: "model".into(),
                        type_id: Some("model".into()),
                        aliases: vec![],
                        mentions: vec!["m1".into()],
                        entity_hk: None,
                    },
                    DocEntity {
                        local_id: "e2".into(),
                        name: "GLUE".into(),
                        raw_type: "dataset".into(),
                        type_id: Some("dataset".into()),
                        aliases: vec![],
                        mentions: vec![],
                        entity_hk: None,
                    },
                ],
                ..Default::default()
            })
        }
    }

    /// Links entities whose context mentions them, creates the rest.
    struct FakeResolver(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl CorpusResolver for FakeResolver {
        async fn resolve(&self, tenant: &TenantId, entity: &DocEntity, context: &str) -> Result<Resolution, ExtractError> {
            self.0.lock().unwrap().push(format!("{}|{context}", entity.name));
            Ok(if context.contains(&entity.name) {
                Resolution::Link { entity_hk: HashKey::of(&["corpus", &entity.name]), score: 0.95 }
            } else {
                Resolution::New { entity_hk: HashKey::of(&[tenant.as_str(), &entity.name]) }
            })
        }
    }

    #[tokio::test]
    async fn stage2_resolves_builds_graph_and_summarises() {
        let doc = HashKey::of(&["d"]);
        let run = RunId::new();
        let stitched = vs_stitch::stitch(vec![page(&doc, run, 1, DocType::ScientificPaper)], 2).unwrap();
        assert_eq!(stitched.missing_pages, vec![2]);
        let run_info = RunInfo {
            tenant: TenantId("t".into()),
            run_id: run,
            language: "en".into(),
            page_count: 2,
            doc_types: vec!["scientific_paper".into()],
            new_entities: HashSet::new(),
        };
        let resolver = FakeResolver(Default::default());
        let out = stage2(&stitched, &spec(), run_info, &FakeExtractor, &resolver, &FakeEmbedder, 2).await.unwrap();

        let calls = resolver.0.lock().unwrap().clone();
        assert!(calls.contains(&"BERT|BERT is a model.".to_string()), "{calls:?}");
        assert!(calls.contains(&"GLUE|".to_string()), "entity without mentions gets empty context: {calls:?}");
        assert_eq!(out.ext.entities[0].entity_hk, Some(HashKey::of(&["corpus", "BERT"])));
        assert_eq!(out.run_info.new_entities.len(), 1, "only GLUE is new");
        assert!(out.graph.nodes.iter().any(|n| n.kind == vs_graph::NodeKind::Entity));
        assert_eq!(out.vectors.by_key.len(), out.graph.embed_targets.len());

        let r = doc_result(&stitched, &out, 2, 1, JobState::Partial);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["state"], "partial");
        assert_eq!(v["pages_failed"], 1);
        assert_eq!(v["missing_pages"], serde_json::json!([2]));
        assert_eq!(v["extraction"]["entities"], 2);
        assert_eq!(v["extraction"]["new_entities"], 1);
        assert_eq!(v["extraction"]["linked_entities"], 1);
        assert_eq!(v["extraction"]["mentions"], 1);
        assert!(v["stitched"]["blocks"].as_array().unwrap().len() == 2);
    }

    #[test]
    fn error_classes() {
        assert!(!extract_error(ExtractError::Model("503".into())).permanent);
        assert!(!extract_error(ExtractError::Store("pg".into())).permanent);
        assert!(extract_error(ExtractError::Schema("bad".into())).permanent);
        assert!(stitch_error(StitchError::Empty).permanent);
        assert!(model_error(ModelError::UnknownModel("x".into())).permanent);
        assert!(!model_error(ModelError::Http("down".into())).permanent);
    }
}
