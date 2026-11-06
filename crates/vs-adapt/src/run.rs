//! `run`: render + classify the dataset, then execute candidates with successive halving.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use vs_classify::{combine, tier0, Classifier, ClassifyConfig};
use vs_core::{content_sha256, DocType, HashKey, LanguageTag, PageProfile, RunId, TenantId};
use vs_dag::{Artifact, Dag, Executor, NodeOutcome, PageImage, StepContext, StepRegistry, TextLayer};
use vs_infra::BlobStore;
use vs_ingest::{detect_format, Format, PageAssets, RenderPool};

use crate::metrics::EvalBlock;
use crate::records::{safe_name, AdaptPageResult, RunRecord, TraceSummary};
use crate::score::{gt_blocks, labelled_metrics, load_ground_truth, quality_from_metrics, MetricWeights};
use crate::select::{class_key, key_string, Objective, Weights};

/// One line of the dataset manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub uri: String,
    pub language: String,
    #[serde(default)]
    pub doc_type_hint: Option<DocType>,
    #[serde(default)]
    pub ground_truth_uri: Option<String>,
    #[serde(default = "default_split")]
    pub split: String,
}

fn default_split() -> String {
    "train".into()
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub out: PathBuf,
    /// Pages per class in the first round.
    pub sample: usize,
    /// Halving rounds; survivors then run on every remaining page.
    pub rounds: usize,
    /// Pages per document.
    pub max_pages: u32,
    pub dpi: u32,
    pub thumb_px: u32,
    /// Concurrent DAG executions.
    pub parallel: usize,
    /// Only documents with this split.
    pub split: Option<String>,
    /// Share of candidates kept per round.
    pub keep_share: f32,
    pub objective: Objective,
    pub weights: MetricWeights,
}

/// A rendered, classified page ready for execution. The page image stays on disk.
#[derive(Debug, Clone)]
pub struct PageCtx {
    pub doc: String,
    pub document_hk: HashKey,
    pub page_no: u32,
    pub image_path: PathBuf,
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: u32,
    pub text_layer: TextLayer,
    pub profile: PageProfile,
    pub class_key: String,
    pub ground_truth_uri: Option<String>,
    pub ground_truth: Option<Vec<EvalBlock>>,
    pub split: String,
}

/// Fetches a document to a local file (local paths are used in place).
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    async fn fetch(&self, uri: &str, spool: &Path) -> anyhow::Result<PathBuf>;
}

pub struct LocalFetcher;

#[async_trait::async_trait]
impl Fetcher for LocalFetcher {
    async fn fetch(&self, uri: &str, _spool: &Path) -> anyhow::Result<PathBuf> {
        let p = PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri));
        if !p.exists() {
            anyhow::bail!("{uri}: no such file (s3:// URIs need --blob-config)");
        }
        Ok(p)
    }
}

/// `s3://bucket/key` through `ObjectBlobStore`; anything else is treated as local.
pub struct BlobFetcher {
    pub store: Arc<dyn BlobStore>,
    pub bucket: String,
}

#[async_trait::async_trait]
impl Fetcher for BlobFetcher {
    async fn fetch(&self, uri: &str, spool: &Path) -> anyhow::Result<PathBuf> {
        if !uri.starts_with("s3://") {
            return LocalFetcher.fetch(uri, spool).await;
        }
        let key = vs_infra::blob::key_from_uri(uri, &self.bucket);
        let bytes = self.store.get(key).await?;
        let local = spool.join(safe_name(key));
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&local, &bytes)?;
        Ok(local)
    }
}

pub struct Runner {
    pub registry: StepRegistry,
    pub pool: Arc<RenderPool>,
    pub classifier: Option<Classifier>,
    pub classify_cfg: ClassifyConfig,
    pub fetcher: Box<dyn Fetcher>,
    pub cfg: RunConfig,
}

/// Document id from the URI: file stem, de-duplicated with a content-hash suffix.
pub fn doc_ids(uris: &[&str], sha: &[String]) -> Vec<String> {
    let stems: Vec<String> = uris
        .iter()
        .map(|u| Path::new(u).file_stem().map(|s| safe_name(&s.to_string_lossy())).filter(|s| !s.is_empty()).unwrap_or_else(|| "doc".into()))
        .collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in &stems {
        *counts.entry(s.as_str()).or_default() += 1;
    }
    stems.iter().zip(sha).map(|(s, h)| if counts[s.as_str()] > 1 { format!("{s}-{}", &h[..8]) } else { s.clone() }).collect()
}

impl Runner {
    fn format_of(path: &Path) -> anyhow::Result<Format> {
        use std::io::Read;
        let mut head = vec![0u8; 2048];
        let n = std::fs::File::open(path)?.read(&mut head)?;
        detect_format(&head[..n]).ok_or_else(|| anyhow::anyhow!("{}: unsupported format", path.display()))
    }

    async fn classify(&self, language: &LanguageTag, assets: &PageAssets, hint: Option<DocType>) -> PageProfile {
        let rules_only = || combine(language.clone(), &assets.signals, tier0(&assets.signals, &self.classify_cfg), None, hint, &self.classify_cfg);
        match &self.classifier {
            Some(c) => match c.classify(language.clone(), &assets.signals, &assets.thumbnail, hint).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(%e, "vision classifier failed; rules only");
                    rules_only()
                }
            },
            None => rules_only(),
        }
    }

    /// Renders and classifies every page of the manifest (page images → `out/pages`).
    pub async fn prepare(&self, manifest: &[ManifestEntry]) -> anyhow::Result<Vec<PageCtx>> {
        let docs: Vec<&ManifestEntry> = manifest.iter().filter(|m| self.cfg.split.as_ref().map_or(true, |s| &m.split == s)).collect();
        let spool = self.cfg.out.join("spool");
        std::fs::create_dir_all(&spool)?;
        let mut paths = vec![];
        let mut shas = vec![];
        for m in &docs {
            let p = self.fetcher.fetch(&m.uri, &spool).await?;
            shas.push(content_sha256(&std::fs::read(&p)?));
            paths.push(p);
        }
        let uris: Vec<&str> = docs.iter().map(|m| m.uri.as_str()).collect();
        let ids = doc_ids(&uris, &shas);
        let tenant = TenantId("vs-adapt".into());

        let mut pages = vec![];
        for (((m, path), sha), id) in docs.iter().zip(&paths).zip(&shas).zip(&ids) {
            let language = LanguageTag::parse(&m.language)?;
            let format = Self::format_of(path)?;
            let count = self.pool.page_count(path, format, self.cfg.max_pages.max(1) * 100).await?;
            let n = count.min(self.cfg.max_pages.max(1));
            let document_hk = HashKey::document(&tenant, sha);
            let page_dir = self.cfg.out.join("pages").join(id);
            std::fs::create_dir_all(&page_dir)?;
            for page_no in 1..=n {
                let assets = match self.pool.page(path, format, count, page_no, self.cfg.dpi, self.cfg.thumb_px).await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(doc = %id, page_no, %e, "render failed; page skipped");
                        continue;
                    }
                };
                let image_path = page_dir.join(format!("{page_no}.png"));
                std::fs::write(&image_path, &assets.image.bytes)?;
                let profile = self.classify(&language, &assets, m.doc_type_hint).await;
                let ground_truth = match &m.ground_truth_uri {
                    Some(uri) => match load_ground_truth(uri, page_no) {
                        Ok(Some(p)) => Some(gt_blocks(&p)),
                        Ok(None) => None,
                        Err(e) => {
                            tracing::warn!(doc = %id, page_no, %e, "ground truth unreadable");
                            None
                        }
                    },
                    None => None,
                };
                pages.push(PageCtx {
                    doc: id.clone(),
                    document_hk: document_hk.clone(),
                    page_no,
                    image_path,
                    width_px: assets.image.width_px,
                    height_px: assets.image.height_px,
                    dpi: assets.image.dpi,
                    text_layer: assets.text_layer,
                    class_key: key_string(&class_key(&profile, 0)),
                    profile,
                    ground_truth_uri: m.ground_truth_uri.clone(),
                    ground_truth,
                    split: m.split.clone(),
                });
            }
            tracing::info!(doc = %id, pages = n, "prepared");
        }
        Ok(pages)
    }

    /// Runs one DAG on one page; never fails (errors are recorded).
    pub async fn execute(&self, dag: &Dag, page: &PageCtx) -> RunRecord {
        let mut rec = RunRecord {
            doc: page.doc.clone(),
            page: page.page_no,
            dag_id: dag.id.clone(),
            dag_version: dag.version.clone(),
            class_key: page.class_key.clone(),
            profile_json: page.profile.clone(),
            latency_ms: 0.0,
            gpu_seconds: 0.0,
            tokens: 0,
            ok: false,
            error: None,
            result_path: None,
            image_path: Some(rel(&page.image_path, &self.cfg.out)),
            ground_truth_uri: page.ground_truth_uri.clone(),
            split: Some(page.split.clone()),
            blocks: 0,
        };
        let bytes = match std::fs::read(&page.image_path) {
            Ok(b) => b,
            Err(e) => {
                rec.error = Some(format!("page image: {e}"));
                return rec;
            }
        };
        let image = Artifact::PageImage(PageImage {
            uri: page.image_path.to_string_lossy().into_owned(),
            bytes: Arc::from(bytes),
            width_px: page.width_px,
            height_px: page.height_px,
            dpi: page.dpi,
        });
        let ctx = StepContext {
            run_id: RunId::new(),
            document_hk: page.document_hk.clone(),
            page_hk: HashKey::page(&page.document_hk, page.page_no),
            page_no: page.page_no,
            profile: page.profile.clone(),
            dag_ref: dag.reference(),
            node_id: String::new(),
        };
        let t0 = Instant::now();
        let res = Executor::new(&self.registry).run(dag, &ctx, image, Artifact::TextLayer(page.text_layer.clone())).await;
        rec.latency_ms = t0.elapsed().as_secs_f32() * 1000.0;
        match res {
            Ok(out) => {
                // No token accounting reaches the executor; GPU time ≈ wall time of model nodes.
                rec.gpu_seconds = out.traces.iter().filter(|t| t.provenance.model_ref.is_some()).map(|t| t.provenance.duration_ms as f32 / 1000.0).sum();
                rec.blocks = out.blocks.len() as u32;
                let result = AdaptPageResult {
                    doc: page.doc.clone(),
                    page_no: page.page_no,
                    dag_ref: dag.reference(),
                    profile: page.profile.clone(),
                    render_width_px: page.width_px,
                    render_height_px: page.height_px,
                    blocks: out.blocks,
                    traces: out
                        .traces
                        .iter()
                        .map(|t| TraceSummary {
                            node_id: t.node_id.clone(),
                            step_kind: t.provenance.step_kind.clone(),
                            model_ref: t.provenance.model_ref.clone(),
                            prompt_ref: t.provenance.prompt_ref.clone(),
                            duration_ms: t.provenance.duration_ms,
                            outcome: match &t.outcome {
                                NodeOutcome::Ok => "ok".into(),
                                NodeOutcome::SkippedAfterError(e) => format!("skipped: {e}"),
                            },
                        })
                        .collect(),
                };
                let dir = self.cfg.out.join("results").join(&page.doc).join(page.page_no.to_string());
                let path = dir.join(format!("{}.json", safe_name(&dag.id)));
                match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, serde_json::to_vec(&result).unwrap_or_default())) {
                    Ok(()) => {
                        rec.ok = true;
                        rec.result_path = Some(rel(&path, &self.cfg.out));
                    }
                    Err(e) => rec.error = Some(format!("writing result: {e}")),
                }
            }
            Err(e) => rec.error = Some(e.to_string()),
        }
        rec
    }

    /// Successive halving over `candidates`; returns every run record.
    pub async fn run_all(&self, candidates: &[Dag], pages: &[PageCtx]) -> anyhow::Result<Vec<RunRecord>> {
        let runs_path = self.cfg.out.join("runs.jsonl");
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&runs_path)?;
        let mut records: Vec<RunRecord> = vec![];
        let mut survivors: Vec<&Dag> = candidates.iter().collect();
        let mut used: HashSet<(String, u32)> = HashSet::new();
        let gts: HashMap<(String, u32), &PageCtx> = pages.iter().map(|p| ((p.doc.clone(), p.page_no), p)).collect();

        let mut round = 0;
        loop {
            let final_round = round >= self.cfg.rounds || survivors.len() <= 1;
            let batch: Vec<&PageCtx> = if final_round {
                pages.iter().filter(|p| !used.contains(&(p.doc.clone(), p.page_no))).collect()
            } else {
                sample_pages(pages, &used, self.cfg.sample * 2usize.pow(round as u32))
            };
            if batch.is_empty() {
                break;
            }
            tracing::info!(round, candidates = survivors.len(), pages = batch.len(), "round");
            let sem = Arc::new(tokio::sync::Semaphore::new(self.cfg.parallel.max(1)));
            let futs = survivors.iter().flat_map(|d| batch.iter().map(move |p| (*d, *p))).map(|(d, p)| {
                let sem = sem.clone();
                async move {
                    let _permit = sem.acquire().await.expect("semaphore open");
                    self.execute(d, p).await
                }
            });
            let new: Vec<RunRecord> = futures::future::join_all(futs).await;
            for r in &new {
                use std::io::Write;
                writeln!(file, "{}", serde_json::to_string(r)?)?;
            }
            for p in &batch {
                used.insert((p.doc.clone(), p.page_no));
            }
            records.extend(new);
            if final_round {
                break;
            }
            // provisional utility over everything run so far
            let w = self.cfg.objective.weights(vs_core::Profile::Balanced);
            let mut sums: HashMap<&str, (f32, usize)> = HashMap::new();
            for r in &records {
                let Some(page) = gts.get(&(r.doc.clone(), r.page)) else { continue };
                let q = provisional_quality(r, page, &self.cfg.weights, &self.cfg.out);
                let u = utility_no_floor(&self.cfg.objective, w, q, r.latency_ms, r.gpu_seconds);
                let s = sums.entry(r.dag_id.as_str()).or_default();
                s.0 += u;
                s.1 += 1;
            }
            let utilities: Vec<(String, f32)> = survivors.iter().map(|d| (d.id.clone(), sums.get(d.id.as_str()).map(|(s, n)| s / *n as f32).unwrap_or(f32::MIN))).collect();
            let keep = shortlist(&utilities, self.cfg.keep_share);
            survivors.retain(|d| keep.contains(&d.id));
            tracing::info!(round, kept = ?keep, "shortlisted");
            round += 1;
        }
        Ok(records)
    }
}

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base).unwrap_or(path).to_string_lossy().into_owned()
}

/// Next `n_per_class` unused pages of every class (deterministic: manifest order).
pub fn sample_pages<'a>(pages: &'a [PageCtx], used: &HashSet<(String, u32)>, n_per_class: usize) -> Vec<&'a PageCtx> {
    let mut per_class: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = vec![];
    for p in pages {
        if used.contains(&(p.doc.clone(), p.page_no)) {
            continue;
        }
        let c = per_class.entry(p.class_key.as_str()).or_default();
        if *c < n_per_class {
            *c += 1;
            out.push(p);
        }
    }
    out
}

/// Quality for shortlisting: labelled quality when the page has ground truth, else a
/// judge-free proxy (block count saturating at 20; 0 for failed runs).
pub fn provisional_quality(rec: &RunRecord, page: &PageCtx, w: &MetricWeights, out_dir: &Path) -> f32 {
    if !rec.ok {
        return 0.0;
    }
    match &page.ground_truth {
        Some(gt) => {
            let result: Option<AdaptPageResult> = rec
                .result_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(out_dir.join(p)).ok())
                .and_then(|t| serde_json::from_str(&t).ok());
            match result {
                Some(r) => quality_from_metrics(&labelled_metrics(&crate::score::draft_blocks(&r.blocks), gt), w),
                None => 0.0,
            }
        }
        None => (rec.blocks as f32 / 20.0).min(1.0),
    }
}

/// Objective utility without the quality floor (the floor would drop every candidate on
/// proxy-scored classes).
pub fn utility_no_floor(obj: &Objective, w: Weights, quality: f32, latency_ms: f32, gpu_seconds: f32) -> f32 {
    w.quality * quality - w.latency * latency_ms / obj.latency_ref_ms - w.cost * gpu_seconds / obj.gpu_ref_seconds
}

/// Top `keep_share` of candidates by utility (at least one; ties → lower id).
pub fn shortlist(utilities: &[(String, f32)], keep_share: f32) -> Vec<String> {
    let mut v: Vec<&(String, f32)> = utilities.iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    let keep = ((v.len() as f32 * keep_share).ceil() as usize).clamp(1, v.len());
    v.into_iter().take(keep).map(|(id, _)| id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::ClassifierTier;

    fn page(doc: &str, no: u32, key: &str) -> PageCtx {
        PageCtx {
            doc: doc.into(),
            document_hk: HashKey::of(&[doc]),
            page_no: no,
            image_path: PathBuf::from("x.png"),
            width_px: 1,
            height_px: 1,
            dpi: 72,
            text_layer: TextLayer::default(),
            profile: PageProfile {
                language: LanguageTag::parse("en").unwrap(),
                doc_type: DocType::Other,
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
            },
            class_key: key.into(),
            ground_truth_uri: None,
            ground_truth: None,
            split: "train".into(),
        }
    }

    #[test]
    fn sampling_is_per_class_and_skips_used() {
        let pages = vec![page("a", 1, "k1"), page("a", 2, "k1"), page("a", 3, "k1"), page("b", 1, "k2")];
        let mut used = HashSet::new();
        let s = sample_pages(&pages, &used, 2);
        assert_eq!(s.iter().map(|p| (p.doc.as_str(), p.page_no)).collect::<Vec<_>>(), vec![("a", 1), ("a", 2), ("b", 1)]);
        used.insert(("a".into(), 1));
        used.insert(("b".into(), 1));
        let s = sample_pages(&pages, &used, 1);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].page_no, 2);
    }

    #[test]
    fn shortlist_keeps_top_third() {
        let u: Vec<(String, f32)> = (0..9).map(|i| (format!("d{i}"), i as f32)).collect();
        assert_eq!(shortlist(&u, 1.0 / 3.0), vec!["d8", "d7", "d6"]);
        assert_eq!(shortlist(&u[..1], 1.0 / 3.0), vec!["d0"]);
        assert_eq!(shortlist(&u[..2], 1.0 / 3.0).len(), 1);
    }

    #[test]
    fn proxy_quality_and_utility() {
        let p = page("a", 1, "k");
        let mut r = RunRecord {
            doc: "a".into(),
            page: 1,
            dag_id: "d".into(),
            dag_version: "v".into(),
            class_key: "k".into(),
            profile_json: p.profile.clone(),
            latency_ms: 2000.0,
            gpu_seconds: 1.0,
            tokens: 0,
            ok: true,
            error: None,
            result_path: None,
            image_path: None,
            ground_truth_uri: None,
            split: None,
            blocks: 10,
        };
        let w = MetricWeights::default();
        let out = Path::new("/nonexistent");
        assert!((provisional_quality(&r, &p, &w, out) - 0.5).abs() < 1e-6);
        r.blocks = 100;
        assert_eq!(provisional_quality(&r, &p, &w, out), 1.0);
        r.ok = false;
        assert_eq!(provisional_quality(&r, &p, &w, out), 0.0);
        let obj = Objective::default();
        let u = utility_no_floor(&obj, obj.balanced_profile, 0.5, 2000.0, 1.0);
        assert!((u - (0.5 - 0.1 - 0.1)).abs() < 1e-6);
    }

    #[test]
    fn doc_ids_dedup() {
        let shas = vec!["aaaaaaaaaaaa".to_string(), "bbbbbbbbbbbb".to_string(), "cccccccccccc".to_string()];
        let ids = doc_ids(&["/x/paper.pdf", "/y/paper.pdf", "s3://b/other.tiff"], &shas);
        assert_eq!(ids, vec!["paper-aaaaaaaa", "paper-bbbbbbbb", "other"]);
    }

    #[test]
    fn manifest_defaults() {
        let m: ManifestEntry = serde_json::from_str(r#"{"uri":"a.pdf","language":"de"}"#).unwrap();
        assert_eq!(m.split, "train");
        assert!(m.doc_type_hint.is_none());
    }
}
