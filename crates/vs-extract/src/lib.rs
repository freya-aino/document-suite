//! Stage 2 information extraction. See `SPEC.md`.

pub mod bib;
pub mod ground;
pub mod model;
pub mod ontology;
pub mod parse;
pub mod resolve;
pub mod tables;
pub mod verify;
pub mod visual;
pub mod window;
#[cfg(test)]
pub(crate) mod test_util;

pub use ground::locate_span;
pub use model::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, ExtractionOutput, Fragment, Mention, Qualifiers};
pub use ontology::{EmbeddingTypeNormalizer, InMemoryOntologyStore, OntologyStore, OntologyType};
pub use resolve::{resolve_in_document, slug, CorpusResolver, NoopTypeNormalizer, Resolution, TypeNormalizer};
pub use window::{build_windows, document_title, render_window, RenderedWindow, Window};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use vs_core::{Evidence, HashKey, Profile, Provenance};
use vs_models::client::ChatResponse;
use vs_models::prompts::Prompt;
use vs_models::{ChatRequest, OpenAiClient};
use vs_stitch::StitchedDocument;

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("model: {0}")]
    Model(String),
    #[error("schema violation: {0}")]
    Schema(String),
    #[error("ontology store: {0}")]
    Store(String),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

/// Full Stage 2 extraction for one document (steps 1–6 and 8 in SPEC.md; corpus resolution is separate).
#[async_trait]
pub trait Extractor: Send + Sync {
    async fn extract(&self, doc: &StitchedDocument, profile: Profile) -> Result<ExtractionOutput, ExtractError>;
}

pub const EXTRACTION_SCHEMA: &str = include_str!("../schemas/extraction.schema.json");
pub const VERIFY_SCHEMA: &str = include_str!("../schemas/verify.schema.json");
pub const EXTRACT_PROMPT: Prompt = Prompt { id: "extract", text: include_str!("../prompts/extract.md") };
pub const VERIFY_PROMPT: Prompt = Prompt { id: "verify", text: include_str!("../prompts/verify.md") };
/// `dag_ref` recorded on Stage 2 provenance.
pub const DAG_REF: &str = "stage2@1";

/// Document-type profile selecting the frames the prompt asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocProfile {
    #[default]
    Scientific,
    Legal,
    Financial,
    Generic,
}

impl DocProfile {
    pub fn frames_text(self) -> &'static str {
        match self {
            DocProfile::Scientific => "\
- result (nary): method, task, dataset, metric, value (literal with unit), condition?
- uses (nary): agent (method/system), resource (dataset/tool/model)
- compares (nary): subject, baseline, metric?, outcome (literal: better/worse/equal)
- proposes (nary): author_or_paper, artifact
- defines (nary): term, definition (literal)
- plus free binary relations (subject/object) and attributes (subject/value) for anything else stated explicitly.",
            DocProfile::Legal => "\
- party_to (binary): subject (person/organisation), object (contract/case)
- obligation (nary): obligor, obligee, action (literal), deadline?, condition?
- defines (nary): term, definition (literal)
- cites (nary): claim_block, reference (law, article, case)
- plus free binary relations (subject/object) and attributes (subject/value).",
            DocProfile::Financial => "\
- amount (nary): item, value (literal with currency unit), period?, entity?
- owns / holds (binary): subject, object
- reports (nary): organisation, metric, value (literal), period
- plus free binary relations (subject/object) and attributes (subject/value).",
            DocProfile::Generic => "\
- free binary relations (subject/object) between named entities
- attributes (subject/value) with literal values (numbers with units, dates, strings)
- defines (nary): term, definition (literal)",
        }
    }
}

/// Stage 2 configuration.
#[derive(Debug, Clone)]
pub struct ExtractConfig {
    /// Registry id of the extraction LLM.
    pub extraction_model: String,
    /// Registry id of the verification LLM (may equal `extraction_model`).
    pub verify_model: String,
    pub window_chars: usize,
    pub drop_ungrounded: bool,
    /// Profiles for which verification runs (default: Quality and Balanced).
    pub verify_profiles: Vec<Profile>,
    pub type_match_threshold: f32,
    /// Concurrent model calls per document.
    pub max_concurrency: usize,
    /// Assertions per verification call.
    pub verify_batch: usize,
    pub max_tokens: u32,
    /// Language hint for prompts (BCP-47 or a name); `None` → "the document's language".
    pub language: Option<String>,
    pub doc_profile: DocProfile,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            extraction_model: String::new(),
            verify_model: String::new(),
            window_chars: 12_000,
            drop_ungrounded: true,
            verify_profiles: vec![Profile::Quality, Profile::Balanced],
            type_match_threshold: 0.85,
            max_concurrency: 8,
            verify_batch: 10,
            max_tokens: 4096,
            language: None,
            doc_profile: DocProfile::Scientific,
        }
    }
}

impl ExtractConfig {
    pub fn new(extraction_model: impl Into<String>, verify_model: impl Into<String>) -> Self {
        Self { extraction_model: extraction_model.into(), verify_model: verify_model.into(), ..Default::default() }
    }

    pub fn verify_for(&self, profile: Profile) -> bool {
        self.verify_profiles.contains(&profile)
    }
}

/// The chat-completion side of `vs_models::OpenAiClient`, abstracted so the orchestration can be
/// tested without a server.
#[async_trait]
pub trait ChatBackend: Send + Sync {
    /// `model_id@revision` for provenance.
    fn model_ref(&self, model: &str) -> Result<String, ExtractError>;
    async fn chat(&self, model: &str, req: &ChatRequest) -> Result<ChatResponse, ExtractError>;
}

#[async_trait]
impl ChatBackend for OpenAiClient {
    fn model_ref(&self, model: &str) -> Result<String, ExtractError> {
        self.spec(model).map(|s| s.model_ref()).map_err(|e| ExtractError::Model(e.to_string()))
    }
    async fn chat(&self, model: &str, req: &ChatRequest) -> Result<ChatResponse, ExtractError> {
        OpenAiClient::chat(self, model, req).await.map_err(|e| ExtractError::Model(e.to_string()))
    }
}

/// LLM-based extractor using `vs-models`.
pub struct LlmExtractor {
    pub backend: Arc<dyn ChatBackend>,
    pub config: ExtractConfig,
    pub normalizer: Arc<dyn TypeNormalizer>,
}

impl LlmExtractor {
    pub fn new(client: Arc<OpenAiClient>, config: ExtractConfig, normalizer: Arc<dyn TypeNormalizer>) -> Self {
        Self { backend: client, config, normalizer }
    }

    pub fn with_backend(backend: Arc<dyn ChatBackend>, config: ExtractConfig, normalizer: Arc<dyn TypeNormalizer>) -> Self {
        Self { backend, config, normalizer }
    }

    fn language(&self) -> &str {
        self.config.language.as_deref().unwrap_or("the document's language")
    }

    #[allow(clippy::too_many_arguments)]
    fn provenance(&self, doc: &StitchedDocument, step_kind: &str, node_id: String, model_ref: Option<String>, prompt_ref: Option<String>, params_hash: String, started_at: chrono::DateTime<chrono::Utc>, duration_ms: u64) -> Provenance {
        Provenance { run_id: doc.run_id, dag_ref: DAG_REF.into(), node_id, step_kind: step_kind.to_string(), model_ref, prompt_ref, params_hash, started_at, duration_ms }
    }

    async fn extract_windows(&self, doc: &StitchedDocument, out: &mut ExtractionOutput, texts: &HashMap<HashKey, &str>) -> Result<(), ExtractError> {
        let title = document_title(doc);
        let windows = build_windows(doc, self.config.window_chars);
        tracing::debug!(windows = windows.len(), "built extraction windows");
        let model = self.config.extraction_model.as_str();
        let model_ref = self.backend.model_ref(model)?;
        let system = EXTRACT_PROMPT.render(&[("language", self.language()), ("profile_frames", self.config.doc_profile.frames_text())]);
        let schema: serde_json::Value = serde_json::from_str(EXTRACTION_SCHEMA).expect("extraction schema is valid JSON");

        let requests: Vec<(usize, RenderedWindow, ChatRequest)> = windows
            .iter()
            .enumerate()
            .map(|(wi, w)| {
                let rendered = render_window(doc, w, title.as_deref());
                let req = ChatRequest { system: Some(system.clone()), prompt: rendered.text(), images: vec![], max_tokens: self.config.max_tokens, json_schema: Some(schema.clone()) };
                (wi, rendered, req)
            })
            .collect();
        let backend = self.backend.clone();
        let model = model.to_string();
        let calls = futures::stream::iter(requests)
            .map(move |(wi, rendered, req)| {
                let backend = backend.clone();
                let model = model.clone();
                async move {
                    let started_at = chrono::Utc::now();
                    let t0 = Instant::now();
                    let resp = backend.chat(&model, &req).await?;
                    let raw = parse::parse_raw(&resp.text)?;
                    Ok::<_, ExtractError>((wi, rendered, raw, started_at, t0.elapsed().as_millis() as u64))
                }
            })
            .buffered(self.config.max_concurrency.max(1))
            .collect::<Vec<_>>()
            .await;

        for call in calls {
            let (wi, rendered, raw, started_at, duration_ms) = call?;
            let idx = out.provenance.len();
            out.provenance.push(self.provenance(
                doc,
                "stage2.extract",
                format!("extract.w{wi}"),
                Some(model_ref.clone()),
                Some(EXTRACT_PROMPT.reference()),
                HashKey::of(&[&rendered.header]).to_string(),
                started_at,
                duration_ms,
            ));
            let g = parse::ground_output(raw, &rendered.labels, texts, &format!("w{wi}"), idx, self.config.drop_ungrounded);
            out.dropped_ungrounded += g.dropped_ungrounded;
            out.absorb(g.fragment);
        }
        Ok(())
    }

    fn add_cpu_fragment(&self, doc: &StitchedDocument, out: &mut ExtractionOutput, step_kind: &str, started_at: chrono::DateTime<chrono::Utc>, t0: Instant, produce: impl FnOnce(usize) -> Fragment) {
        let idx = out.provenance.len();
        let frag = produce(idx);
        if frag.is_empty() {
            return;
        }
        out.provenance.push(self.provenance(doc, step_kind, step_kind.replace("stage2.", ""), None, None, HashKey::of(&[DAG_REF, step_kind]).to_string(), started_at, t0.elapsed().as_millis() as u64));
        out.absorb(frag);
    }

    async fn verify(&self, doc: &StitchedDocument, out: &mut ExtractionOutput, texts: &HashMap<HashKey, &str>) -> Result<(), ExtractError> {
        let candidates: Vec<usize> = out
            .assertions
            .iter()
            .enumerate()
            .filter(|(_, a)| out.provenance.get(a.provenance_idx).is_some_and(|p| p.step_kind == "stage2.extract") && a.evidence.iter().any(|e| matches!(e, Evidence::Span { .. })))
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let model = self.config.verify_model.as_str();
        let model_ref = self.backend.model_ref(model)?;
        let schema: serde_json::Value = serde_json::from_str(VERIFY_SCHEMA).expect("verify schema is valid JSON");
        let requests: Vec<(usize, String, ChatRequest)> = candidates
            .chunks(self.config.verify_batch.max(1))
            .enumerate()
            .map(|(bi, batch)| {
                let items: Vec<&Assertion> = batch.iter().map(|&i| &out.assertions[i]).collect();
                let rendered = verify::render_items(&items, &out.entities, texts);
                let prompt = VERIFY_PROMPT.render(&[("language", self.language()), ("items", &rendered)]);
                let req = ChatRequest { system: None, prompt, images: vec![], max_tokens: self.config.max_tokens.min(2048), json_schema: Some(schema.clone()) };
                (bi, rendered, req)
            })
            .collect();
        let backend = self.backend.clone();
        let model = model.to_string();
        let calls = futures::stream::iter(requests)
            .map(move |(bi, rendered, req)| {
                let backend = backend.clone();
                let model = model.clone();
                async move {
                    let started_at = chrono::Utc::now();
                    let t0 = Instant::now();
                    let resp = backend.chat(&model, &req).await?;
                    let verdicts = verify::parse_verdicts(&resp.text)?;
                    Ok::<_, ExtractError>((bi, rendered, verdicts, started_at, t0.elapsed().as_millis() as u64))
                }
            })
            .buffered(self.config.max_concurrency.max(1))
            .collect::<Vec<_>>()
            .await;

        let mut verdicts: HashMap<String, bool> = HashMap::new();
        for call in calls {
            let (bi, rendered, v, started_at, duration_ms) = call?;
            out.provenance.push(self.provenance(doc, "stage2.verify", format!("verify.b{bi}"), Some(model_ref.clone()), Some(VERIFY_PROMPT.reference()), HashKey::of(&[&rendered]).to_string(), started_at, duration_ms));
            verdicts.extend(v);
        }
        let mut dropped = 0u32;
        out.assertions.retain_mut(|a| match verdicts.get(&a.local_id) {
            Some(true) => {
                a.verified = true;
                true
            }
            Some(false) => {
                dropped += 1;
                false
            }
            None => true,
        });
        out.dropped_unverified += dropped;
        Ok(())
    }

    async fn normalise_types(&self, out: &mut ExtractionOutput) -> Result<(), ExtractError> {
        let raw_types: HashSet<String> = out.entities.iter().map(|e| e.raw_type.trim().to_string()).collect();
        let mut type_ids: HashMap<String, String> = HashMap::new();
        for raw in raw_types {
            let id = self.normalizer.entity_type(&raw).await?;
            type_ids.insert(raw, id);
        }
        for e in &mut out.entities {
            e.type_id = type_ids.get(e.raw_type.trim()).cloned();
        }
        let mut roles_of: HashMap<String, Vec<String>> = HashMap::new();
        for a in &out.assertions {
            let roles = roles_of.entry(a.predicate.trim().to_string()).or_default();
            for arg in &a.args {
                if !roles.contains(&arg.role) {
                    roles.push(arg.role.clone());
                }
            }
        }
        let mut predicate_ids: HashMap<String, String> = HashMap::new();
        for (raw, roles) in &roles_of {
            let id = self.normalizer.predicate_with_roles(raw, roles).await?;
            predicate_ids.insert(raw.clone(), id);
        }
        for a in &mut out.assertions {
            a.predicate_id = predicate_ids.get(a.predicate.trim()).cloned();
        }
        Ok(())
    }
}

#[async_trait]
impl Extractor for LlmExtractor {
    async fn extract(&self, doc: &StitchedDocument, profile: Profile) -> Result<ExtractionOutput, ExtractError> {
        let texts: HashMap<HashKey, &str> = doc.blocks.iter().map(|b| (b.block.block_hk.clone(), b.block.text.as_str())).collect();
        let mut out = ExtractionOutput::default();

        // 1–3: windows → model → grounding
        self.extract_windows(doc, &mut out, &texts).await?;

        // CPU sources: tables, bibliography + citations, structured figure descriptions
        let (started_at, t0) = (chrono::Utc::now(), Instant::now());
        self.add_cpu_fragment(doc, &mut out, "stage2.tables", started_at, t0, |idx| tables::extract_tables(doc, idx));
        let (started_at, t0) = (chrono::Utc::now(), Instant::now());
        self.add_cpu_fragment(doc, &mut out, "stage2.bibliography", started_at, t0, |idx| bib::extract_bibliography(doc, idx).0);
        let (started_at, t0) = (chrono::Utc::now(), Instant::now());
        self.add_cpu_fragment(doc, &mut out, "stage2.figures", started_at, t0, |idx| visual::extract_figures(doc, idx));

        // 8: visual evidence
        visual::regionalise(&mut out, doc);

        // 4: verification
        if self.config.verify_for(profile) {
            self.verify(doc, &mut out, &texts).await?;
        }

        // 5: in-document resolution
        resolve_in_document(&mut out, doc);

        // 6: type normalisation
        self.normalise_types(&mut out).await?;

        tracing::debug!(
            entities = out.entities.len(),
            assertions = out.assertions.len(),
            dropped_ungrounded = out.dropped_ungrounded,
            dropped_unverified = out.dropped_unverified,
            "stage 2 extraction done"
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::DocBuilder;
    use std::sync::Mutex;
    use vs_core::{BlockContent, BlockKind};

    /// Fake backend: answers by substring match on the prompt, records requests.
    struct FakeBackend {
        rules: Vec<(&'static str, String)>,
        requests: Mutex<Vec<(String, ChatRequest)>>,
    }

    #[async_trait]
    impl ChatBackend for FakeBackend {
        fn model_ref(&self, model: &str) -> Result<String, ExtractError> {
            Ok(format!("{model}@r1"))
        }
        async fn chat(&self, model: &str, req: &ChatRequest) -> Result<ChatResponse, ExtractError> {
            self.requests.lock().unwrap().push((model.to_string(), req.clone()));
            let text = self
                .rules
                .iter()
                .find(|(needle, _)| req.prompt.contains(needle))
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| r#"{"entities":[],"assertions":[]}"#.to_string());
            Ok(ChatResponse { text, prompt_tokens: 1, completion_tokens: 1 })
        }
    }

    fn paper() -> DocBuilder {
        let mut b = DocBuilder::new();
        b.block(BlockKind::Title, "Grounded Extraction");
        b.block(BlockKind::Heading, "1 Introduction");
        b.block(BlockKind::Paragraph, "We fine-tune BERT [1] on the GLUE benchmark. Bidirectional Encoder Representations from Transformers (BERT) is strong.");
        b.block(BlockKind::Heading, "2 Results");
        let p = b.block(BlockKind::Paragraph, "Table 1 shows BERT reaches 92.1 accuracy on GLUE, beating RoBERTa.");
        let t = b.block_with(
            BlockKind::Table,
            "Model Acc (%)\nBERT 92.1\nRoBERTa 90.0",
            BlockContent::Table { html: "<table><tr><th>Model</th><th>Acc (%)</th></tr><tr><td>BERT</td><td>92.1</td></tr><tr><td>RoBERTa</td><td>90.0</td></tr></table>".into() },
        );
        b.link(vs_stitch::LinkKind::RefersTo, p, t);
        b.block(BlockKind::Heading, "References");
        b.block(BlockKind::Paragraph, "[1] J. Devlin et al. BERT: Pre-training of deep bidirectional transformers. In NAACL, 2019.");
        b
    }

    const INTRO: &str = r#"{"entities":[
        {"id":"e1","name":"BERT","type":"method","mentions":[{"block":"B1","quote":"fine-tune BERT [1]"}]},
        {"id":"e2","name":"GLUE benchmark","type":"dataset","mentions":[{"block":"B1","quote":"the GLUE benchmark"}]},
        {"id":"e3","name":"Bidirectional Encoder Representations from Transformers","type":"method","mentions":[{"block":"B1","quote":"Bidirectional Encoder Representations from Transformers (BERT)"}]}],
      "assertions":[
        {"predicate":"uses","kind":"binary","args":[{"role":"subject","entity":"e1","literal":null},{"role":"object","entity":"e2","literal":null}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B1","quote":"We fine-tune BERT [1] on the GLUE benchmark."}],"confidence":0.9}]}"#;

    const RESULTS: &str = r#"{"entities":[
        {"id":"e1","name":"BERT","type":"Method","mentions":[{"block":"B1","quote":"shows BERT reaches"}]},
        {"id":"e2","name":"GLUE","type":"dataset","mentions":[{"block":"B1","quote":"on GLUE,"}]},
        {"id":"e3","name":"RoBERTa","type":"method","mentions":[{"block":"B1","quote":"beating RoBERTa"}]},
        {"id":"e4","name":"Phantom","type":"method","mentions":[{"block":"B1","quote":"not present"}]}],
      "assertions":[
        {"predicate":"result","kind":"nary","args":[{"role":"method","entity":"e1","literal":null},{"role":"dataset","entity":"e2","literal":null},{"role":"metric","entity":null,"literal":{"value":"accuracy","unit":null}},{"role":"value","entity":null,"literal":{"value":"92.1","unit":null}}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B1","quote":"BERT reaches 92.1 accuracy on GLUE"}],"confidence":0.95},
        {"predicate":"compares","kind":"nary","args":[{"role":"subject","entity":"e1","literal":null},{"role":"baseline","entity":"e3","literal":null},{"role":"outcome","entity":null,"literal":{"value":"better","unit":null}}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B1","quote":"beating RoBERTa"}],"confidence":0.8},
        {"predicate":"loses_to","kind":"binary","args":[{"role":"subject","entity":"e1","literal":null},{"role":"object","entity":"e3","literal":null}],
         "qualifiers":{"negated":false,"hedged":false,"condition":null,"time":null},"evidence":[{"block":"B1","quote":"beating RoBERTa"}],"confidence":0.4}]}"#;

    fn backend() -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            rules: vec![
                ("Section: Grounded Extraction > 1 Introduction", INTRO.into()),
                ("Section: Grounded Extraction > 2 Results", RESULTS.into()),
                // verify: every id supported except the contradicted one
                ("id: w1.a3", r#"{"verdicts":[{"id":"w1.a1","supported":true},{"id":"w1.a2","supported":true},{"id":"w1.a3","supported":false},{"id":"w0.a1","supported":true}]}"#.into()),
            ],
            requests: Mutex::new(vec![]),
        })
    }

    #[tokio::test]
    async fn full_pipeline_balanced() {
        let be = backend();
        let ex = LlmExtractor::with_backend(be.clone(), ExtractConfig::new("llm", "llm"), Arc::new(NoopTypeNormalizer));
        let doc = paper().doc;
        let out = ex.extract(&doc, Profile::Balanced).await.unwrap();

        // calls: 2 windows + 1 verify batch
        let reqs = be.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(reqs[0].1.json_schema.is_some());
        assert!(reqs[0].1.system.as_deref().unwrap().contains("result (nary)"));
        assert!(reqs[0].1.prompt.contains("[B1] We fine-tune BERT"));
        // window 2 carries the referred table as context
        assert!(reqs[1].1.prompt.contains("Referenced material:\n[B2] (table) Model Acc (%)"));
        drop(reqs);

        // provenance: 2 extract + tables + bibliography + 1 verify
        let kinds: Vec<&str> = out.provenance.iter().map(|p| p.step_kind.as_str()).collect();
        assert_eq!(kinds, vec!["stage2.extract", "stage2.extract", "stage2.tables", "stage2.bibliography", "stage2.verify"]);
        assert_eq!(out.provenance[0].model_ref.as_deref(), Some("llm@r1"));
        assert_eq!(out.provenance[0].prompt_ref.as_deref(), Some(EXTRACT_PROMPT.reference().as_str()));
        assert_eq!(out.provenance[0].dag_ref, DAG_REF);
        assert!(out.provenance[2].model_ref.is_none());

        // grounding: Phantom dropped (mention + entity)
        assert_eq!(out.dropped_ungrounded, 2);
        // verification: loses_to dropped, others verified
        assert_eq!(out.dropped_unverified, 1);
        assert!(out.assertions.iter().filter(|a| a.predicate != "result" || a.local_id.starts_with('w')).filter(|a| a.local_id.starts_with('w')).all(|a| a.verified));
        assert!(!out.assertions.iter().any(|a| a.predicate == "loses_to"));

        // in-doc resolution: BERT (w0,w1,table,abbrev long form) merged; GLUE benchmark + GLUE stay apart (different names)
        let bert = out.entities.iter().find(|e| e.name == "Bidirectional Encoder Representations from Transformers").expect("long form chosen as name");
        assert!(bert.aliases.contains(&"BERT".to_string()));
        assert_eq!(out.entities.iter().filter(|e| e.name.eq_ignore_ascii_case("bert")).count(), 0);
        assert!(bert.mentions.len() >= 3);
        assert_eq!(bert.type_id.as_deref(), Some("method"));
        // every assertion arg points at a surviving entity
        let ids: HashSet<&str> = out.entities.iter().map(|e| e.local_id.as_str()).collect();
        for a in &out.assertions {
            for arg in &a.args {
                if let ArgValue::Entity { local_id } = &arg.value {
                    assert!(ids.contains(local_id.as_str()), "dangling {local_id}");
                }
            }
            assert!(a.predicate_id.is_some());
        }
        // table frames and citation
        assert!(out.assertions.iter().any(|a| a.predicate == "result" && a.local_id.starts_with("tbl")));
        let cite = out.assertions.iter().find(|a| a.predicate == "cites").expect("citation linked");
        assert_eq!(cite.predicate_id.as_deref(), Some("cites"));
        let reference = out.entities.iter().find(|e| e.raw_type == "reference").unwrap();
        assert_eq!(reference.name, "BERT: Pre-training of deep bidirectional transformers");
        assert_eq!(cite.args[1].value, ArgValue::Entity { local_id: reference.local_id.clone() });
    }

    #[tokio::test]
    async fn fast_profile_skips_verification() {
        let be = backend();
        let ex = LlmExtractor::with_backend(be.clone(), ExtractConfig::new("llm", "llm"), Arc::new(NoopTypeNormalizer));
        let out = ex.extract(&paper().doc, Profile::Fast).await.unwrap();
        assert_eq!(be.requests.lock().unwrap().len(), 2);
        assert_eq!(out.dropped_unverified, 0);
        assert!(out.assertions.iter().all(|a| !a.verified));
        assert!(out.assertions.iter().any(|a| a.predicate == "loses_to"));
    }

    #[tokio::test]
    async fn bad_model_json_is_a_schema_error() {
        let be = Arc::new(FakeBackend { rules: vec![("Section:", "not json".into())], requests: Mutex::new(vec![]) });
        let ex = LlmExtractor::with_backend(be, ExtractConfig::new("llm", "llm"), Arc::new(NoopTypeNormalizer));
        assert!(matches!(ex.extract(&paper().doc, Profile::Fast).await, Err(ExtractError::Schema(_))));
    }
}
