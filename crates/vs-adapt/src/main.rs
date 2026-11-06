//! vs-adapt CLI. See `SPEC.md`.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vs_adapt::run::{BlobFetcher, Fetcher, LocalFetcher, ManifestEntry, RunConfig, Runner};
use vs_adapt::{generate, read_jsonl, report, select, write_jsonl, Judge, MetricWeights, Objective, PageEval, RunRecord, ScoreOptions, SearchSpace};
use vs_models::{ModelRegistry, OpenAiClient};

#[derive(Parser)]
#[command(about = "Generate, run, score and select Stage 1 DAGs (dev/stage only)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Expand templates into validated candidate DAGs.
    Generate {
        #[arg(long)]
        search_space: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Model registry TOML (the step catalogue is built from it).
        #[arg(long, default_value = "config/models.example.toml")]
        models: PathBuf,
    },
    /// Execute candidates on a dataset (stage cluster) with successive halving.
    Run {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        candidates: PathBuf,
        /// Output directory: pages/, results/, runs.jsonl.
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "config/models.example.toml")]
        models: PathBuf,
        /// Pages per class in the first round.
        #[arg(long, default_value_t = 4)]
        sample: usize,
        /// Halving rounds before the survivors run on all remaining pages.
        #[arg(long, default_value_t = 3)]
        rounds: usize,
        /// Pages per document.
        #[arg(long, default_value_t = 50)]
        max_pages: u32,
        /// Render child binary (default: `vs-render-child` next to this executable).
        #[arg(long)]
        render_child: Option<PathBuf>,
        /// Model id of the tier 1 vision classifier; without it pages are classified by rules only.
        #[arg(long)]
        classifier_model: Option<String>,
        /// TOML with `bucket`, `endpoint`, `region` for `s3://` manifest URIs.
        #[arg(long)]
        blob_config: Option<PathBuf>,
        /// Only documents of this split (`train` / `holdout`).
        #[arg(long)]
        split: Option<String>,
        #[arg(long, default_value_t = 200)]
        dpi: u32,
        /// Concurrent DAG executions.
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// Render pool size (default: CPUs).
        #[arg(long)]
        render_pool: Option<usize>,
        #[arg(long)]
        objective: Option<PathBuf>,
        /// Metric weights JSON (see `MetricWeights`).
        #[arg(long)]
        weights: Option<PathBuf>,
    },
    /// Compute PageEval records from runs and ground truth / judge.
    Score {
        #[arg(long)]
        runs: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Metric weights JSON (see `MetricWeights`).
        #[arg(long)]
        weights: Option<PathBuf>,
        /// Judge VLM model id for pages without ground truth.
        #[arg(long)]
        judge_model: Option<String>,
        #[arg(long, default_value = "config/models.example.toml")]
        models: PathBuf,
    },
    /// Pick DAGs per class and write the DAG list plus report.md.
    Select {
        #[arg(long)]
        evals: PathBuf,
        #[arg(long)]
        candidates: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        objective: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
        /// Currently active DAG list JSON, for the comparison section of the report.
        #[arg(long)]
        current: Option<PathBuf>,
        /// Report path (default: `report.md` next to `--out`).
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Upload a DAG list as candidate to the API.
    Promote {
        #[arg(long)]
        dag_list: PathBuf,
        /// API base URL, e.g. `http://localhost:8080`.
        #[arg(long)]
        api: String,
        #[arg(long, env = "VS_TENANT")]
        tenant: String,
    },
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn client(models: &Path) -> anyhow::Result<Arc<OpenAiClient>> {
    let reg = ModelRegistry::load(models).map_err(|e| anyhow::anyhow!("models {}: {e}", models.display()))?;
    Ok(Arc::new(OpenAiClient::new(reg)))
}

fn opt_json<T: serde::de::DeserializeOwned + Default>(path: Option<PathBuf>) -> anyhow::Result<T> {
    match path {
        Some(p) => read_json(&p),
        None => Ok(T::default()),
    }
}

#[derive(serde::Deserialize)]
struct BlobFile {
    bucket: String,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    region: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    match Cli::parse().cmd {
        Cmd::Generate { search_space, out, models } => {
            let space: SearchSpace = read_json(&search_space)?;
            let registry = vs_steps::production_registry(client(&models)?);
            let g = generate(&space, &registry)?;
            std::fs::write(&out, serde_json::to_string_pretty(&g.candidates)?)?;
            println!("wrote {} with {} candidates ({} rejected, {} over max_candidates)", out.display(), g.candidates.len(), g.rejected.len(), g.truncated);
            for r in &g.rejected {
                println!("  rejected {}: {}", r.id, r.error);
            }
            Ok(())
        }
        Cmd::Run {
            manifest,
            candidates,
            out,
            models,
            sample,
            rounds,
            max_pages,
            render_child,
            classifier_model,
            blob_config,
            split,
            dpi,
            parallel,
            render_pool,
            objective,
            weights,
        } => {
            let manifest: Vec<ManifestEntry> = read_jsonl(&manifest)?;
            let dags: Vec<vs_dag::Dag> = read_json(&candidates)?;
            std::fs::create_dir_all(&out)?;
            let client = client(&models)?;
            let registry = vs_steps::production_registry(client.clone());
            for d in &dags {
                d.validate(&registry).map_err(|e| anyhow::anyhow!("candidate {}: {e}", d.id))?;
            }
            let child = match render_child {
                Some(p) => p,
                None => std::env::current_exe()?.parent().map(|d| d.join("vs-render-child")).ok_or_else(|| anyhow::anyhow!("cannot locate vs-render-child"))?,
            };
            let size = render_pool.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2));
            let pool = Arc::new(vs_ingest::RenderPool::new(
                vs_ingest::ChildCommand { program: child, args: vec![], env: vec![] },
                vs_ingest::PoolConfig { size, page_timeout: Duration::from_secs(60), open_timeout: Duration::from_secs(60) },
            ));
            let classify_cfg = vs_classify::ClassifyConfig::default();
            let classifier = classifier_model.map(|model| vs_classify::Classifier {
                cfg: classify_cfg.clone(),
                vision: Arc::new(vs_models::classifier::VlmPageClassifier { client: client.clone(), model }),
            });
            let fetcher: Box<dyn Fetcher> = match blob_config {
                Some(p) => {
                    let f: BlobFile = toml::from_str(&std::fs::read_to_string(&p)?)?;
                    let cfg = vs_infra::config::BlobConfig { bucket: f.bucket.clone(), endpoint: f.endpoint, region: f.region };
                    Box::new(BlobFetcher { store: Arc::new(vs_infra::ObjectBlobStore::s3(&cfg)?), bucket: f.bucket })
                }
                None => Box::new(LocalFetcher),
            };
            let runner = Runner {
                registry,
                pool,
                classifier,
                classify_cfg,
                fetcher,
                cfg: RunConfig {
                    out: out.clone(),
                    sample,
                    rounds,
                    max_pages,
                    dpi,
                    thumb_px: vs_classify::ClassifyConfig::default().thumbnail_long_side_px,
                    parallel,
                    split,
                    keep_share: 1.0 / 3.0,
                    objective: opt_json(objective)?,
                    weights: opt_json(weights)?,
                },
            };
            let pages = runner.prepare(&manifest).await?;
            println!("prepared {} pages from {} documents", pages.len(), manifest.len());
            let records = runner.run_all(&dags, &pages).await?;
            let ok = records.iter().filter(|r| r.ok).count();
            println!("wrote {} runs ({} ok, {} failed) to {}", records.len(), ok, records.len() - ok, out.join("runs.jsonl").display());
            Ok(())
        }
        Cmd::Score { runs, out, weights, judge_model, models } => {
            let records: Vec<RunRecord> = read_jsonl(&runs)?;
            let weights: MetricWeights = opt_json(weights)?;
            let judge = match judge_model {
                Some(model) => Some(Judge { client: client(&models)?, model }),
                None => None,
            };
            let base = runs.parent().map(Path::to_path_buf);
            let evals = vs_adapt::score::score(records, &ScoreOptions { weights, judge: judge.as_ref(), base: base.as_deref() }).await?;
            write_jsonl(&out, &evals)?;
            let labelled = evals.iter().filter(|e| e.trust == vs_adapt::Trust::Labelled).count();
            println!("wrote {} evals ({} labelled, {} judge) to {}", evals.len(), labelled, evals.len() - labelled, out.display());
            Ok(())
        }
        Cmd::Select { evals, candidates, version, objective, out, current, report: report_path } => {
            let evals: Vec<PageEval> = read_jsonl(&evals)?;
            let dags: Vec<vs_dag::Dag> = read_json(&candidates)?;
            let obj: Objective = opt_json(objective)?;
            let list = select(&evals, dags, &obj, &version, Some(format!("vs-adapt select {}", evals.len())))
                .ok_or_else(|| anyhow::anyhow!("no DAG reaches the quality floor"))?;
            std::fs::write(&out, serde_json::to_string_pretty(&list)?)?;
            let current: Option<vs_router::DagList> = match current {
                Some(p) => Some(read_json(&p)?),
                None => None,
            };
            let report_path = report_path.unwrap_or_else(|| out.parent().unwrap_or(Path::new(".")).join("report.md"));
            std::fs::write(&report_path, report::render(&list, &evals, &obj, current.as_ref()))?;
            println!("wrote {} with {} rules and {}", out.display(), list.routing.rules.len(), report_path.display());
            Ok(())
        }
        Cmd::Promote { dag_list, api, tenant } => {
            let list: vs_router::DagList = read_json(&dag_list)?;
            let url = format!("{}/v1/dag-lists", api.trim_end_matches('/'));
            let resp = reqwest::Client::new().post(&url).header("X-VS-Tenant", &tenant).header("X-VS-Admin", "1").json(&list).send().await?;
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            println!("{status} {url}\n{body}");
            if !status.is_success() {
                anyhow::bail!("upload failed with {status}");
            }
            Ok(())
        }
    }
}
