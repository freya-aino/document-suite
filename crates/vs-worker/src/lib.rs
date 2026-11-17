//! Worker roles as a library. See `SPEC.md`.
//!
//! The role handlers and their wiring live here rather than in `main.rs` so that a test
//! can build the same handler a deployment runs. Without that, "nothing has run end to
//! end" was not a gap anyone could close: the three handlers were only ever constructed
//! inside a binary, so the handover between stages was reachable only through fakes.
//! `main.rs` is now the CLI and the signal handling, and nothing else.

pub mod dags;
pub mod doc;
pub mod images;
pub mod ingest;
pub mod page;
pub mod runtime;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use vs_infra::queue::{Role, TaskBus};
use vs_infra::{Config, ObjectBlobStore};
use vs_models::{Embedder, ModelRegistry, OpenAiClient, OpenAiEmbedder};
use vs_vault::{ops::Ops, PgCorpusResolver, PgOntologyStore, PgVault};
use vs_vectors::{QdrantConfig, QdrantStore, VectorStore};

/// Refuses a document this long before rendering any of it.
pub const MAX_PAGES: u32 = 5_000;

/// Everything a handler needs that does not come out of the config file.
pub struct Deps {
    pub bus: Arc<dyn TaskBus>,
    pub pool: sqlx::PgPool,
    /// The command the `ingest` role spawns per render child. In production this binary
    /// with `render-child` (`self_render_command`); a test has to pass the built
    /// `vs-worker` explicitly, because its own `current_exe` is the test binary and does
    /// not speak that protocol.
    pub render_command: vs_ingest::ChildCommand,
}

/// Model registry and client; `page` and `doc` roles only (ingest calls no model).
pub struct Models {
    pub client: Arc<OpenAiClient>,
}

impl Models {
    /// `blob` backs `image_transport = "url"`: a model that asks for links needs
    /// somewhere to put the image, and the registry decides per model whether it does.
    pub fn load(cfg: &Config, blob: Arc<dyn vs_infra::BlobStore>) -> anyhow::Result<Self> {
        let path = &cfg.models.registry;
        let registry = ModelRegistry::load(path).map_err(|e| anyhow::anyhow!("model registry {}: {e}", path.display()))?;
        let by_url = registry.all().filter(|m| m.images_by_url()).count();
        tracing::info!(
            registry = %path.display(),
            models = registry.all().count(),
            images_by_url = by_url,
            "model registry loaded"
        );
        let client = OpenAiClient::new(registry).with_image_host(Arc::new(images::BlobImageHost { blob }));
        Ok(Self { client: Arc::new(client) })
    }
}

/// Builds the handler for `role`, validating everything it needs up front: a missing
/// model, an unreachable Qdrant or a wrong embedding dimension is a startup failure and
/// not a per-document one.
pub async fn build_handler(cfg: &Config, role: Role, deps: Deps) -> anyhow::Result<Arc<dyn runtime::Handler>> {
    let Deps { bus, pool, render_command } = deps;
    let blob = Arc::new(ObjectBlobStore::s3(&cfg.blob)?);
    let ops = Arc::new(Ops { pool: pool.clone() });
    let vault = Arc::new(PgVault { pool });

    Ok(match role {
        Role::Ingest => Arc::new(ingest::IngestHandler {
            blob,
            bus,
            vault,
            ops,
            pool: Arc::new(render_pool(cfg, render_command)?),
            scratch_dir: cfg.worker.scratch_dir.clone().unwrap_or_else(std::env::temp_dir),
            bucket: cfg.blob.bucket.clone(),
            render_dpi: cfg.worker.render_dpi,
            // From `[classify]`, not a constant here: ingest renders the thumbnail and
            // the `page` role consumes it, so the size has to come from one place or the
            // two roles can disagree about what they are passing each other.
            thumbnail_px: classify_config(cfg)?.thumbnail_long_side_px,
            max_pages: MAX_PAGES,
        }),
        Role::Page => {
            let models = Models::load(cfg, blob.clone())?;
            let steps = Arc::new(vs_steps::production_registry(models.client.clone()));
            let classify = classify_config(cfg)?;
            let vision: Option<Arc<dyn vs_classify::VisionClassifier>> = match &cfg.stage2.classifier_model {
                Some(model) => {
                    models.client.spec(model)?; // fail at startup, not per page
                    Some(Arc::new(vs_models::classifier::VlmPageClassifier { client: models.client.clone(), model: model.clone() }))
                }
                None => {
                    tracing::warn!("no stage2.classifier_model configured; pages are classified by rules only");
                    None
                }
            };
            let refresh = Duration::from_secs(cfg.stage2.dag_list_refresh_s);
            Arc::new(page::PageHandler {
                blob,
                bus,
                vault,
                dags: Arc::new(dags::DagLists::new(ops.clone(), steps.clone(), refresh)),
                ops,
                steps,
                classify,
                vision,
                bucket: cfg.blob.bucket.clone(),
                render_dpi: cfg.worker.render_dpi,
            })
        }
        Role::Doc => {
            let models = Models::load(cfg, blob.clone())?;
            let s2 = &cfg.stage2;
            for m in [&s2.extraction_model, &s2.verify_model].into_iter().chain(s2.judge_model.as_ref()) {
                models.client.spec(m)?;
            }
            let embedder = Arc::new(OpenAiEmbedder::new(models.client.clone(), &s2.embedding_model, s2.embed_batch_size)?);
            let vectors: Arc<dyn VectorStore> = Arc::new(QdrantStore::new(&QdrantConfig {
                url: cfg.qdrant.url.clone(),
                api_key: cfg.qdrant.api_key_env.as_deref().and_then(|v| std::env::var(v).ok()),
                collection_prefix: cfg.qdrant.collection_prefix.clone(),
                batch_size: cfg.qdrant.batch_size,
            })?);
            // Collections are sized by the embedder, so this must happen after it is
            // built and before the first document: a wrong dimension is unrecoverable
            // once points are written.
            vectors.ensure_ready(embedder.dimensions()).await?;

            let doc_classifier: Option<Arc<dyn vs_classify::DocumentClassifier>> = match &s2.doc_classifier_model {
                Some(model) => {
                    models.client.spec(model)?; // fail at startup, not per document
                    Some(Arc::new(vs_models::classifier::VlmDocumentClassifier {
                        client: models.client.clone(),
                        model: model.clone(),
                        max_pages: 3,
                    }))
                }
                None => {
                    tracing::info!("no doc_classifier_model: document type comes from the weighted page vote");
                    None
                }
            };
            let mut resolver = PgCorpusResolver::new(ops.pool.clone(), vectors.clone(), embedder.clone())
                .with_thresholds(s2.link_threshold, s2.judge_band);
            if let Some(judge) = &s2.judge_model {
                resolver = resolver.with_judge(models.client.clone(), judge.clone());
            }
            Arc::new(doc::DocHandler {
                blob,
                vault,
                ontology: Arc::new(PgOntologyStore::new(ops.pool.clone())),
                vectors,
                ops,
                client: models.client,
                embedder,
                resolver: Arc::new(resolver),
                doc_classifier,
                stage2: s2.clone(),
                max_failed_page_fraction: cfg.worker.max_failed_page_fraction,
            })
        }
    })
}

/// The `[classify]` section. Read by the `ingest` role for the thumbnail size and by the
/// `page` role for everything else, so a mismatch is impossible by construction.
pub fn classify_config(cfg: &Config) -> anyhow::Result<vs_classify::ClassifyConfig> {
    cfg.classify.clone().try_into().map_err(|e| anyhow::anyhow!("[classify] section: {e}"))
}

/// Render children are this binary with `render-child`.
pub fn self_render_command(cfg: &Config) -> anyhow::Result<vs_ingest::ChildCommand> {
    let env = cfg
        .worker
        .render_mem_limit_mb
        .map(|mb| vec![(vs_ingest::child::MEM_LIMIT_ENV.to_string(), mb.to_string())])
        .unwrap_or_default();
    Ok(vs_ingest::ChildCommand { program: std::env::current_exe()?, args: vec!["render-child".into()], env })
}

pub fn render_pool(cfg: &Config, cmd: vs_ingest::ChildCommand) -> anyhow::Result<vs_ingest::RenderPool> {
    let w = &cfg.worker;
    let size = w.render_processes.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2));
    let timeout = Duration::from_secs(w.page_render_timeout_s);
    tracing::info!(size, ?timeout, "render pool");
    Ok(vs_ingest::RenderPool::new(cmd, vs_ingest::PoolConfig { size, page_timeout: timeout, open_timeout: timeout }))
}

pub async fn connect_pg(cfg: &Config) -> anyhow::Result<sqlx::PgPool> {
    let mut opts = PgConnectOptions::from_str(&cfg.postgres.url)?;
    if let Some(pw) = &cfg.postgres.password {
        opts = opts.password(pw);
    }
    Ok(PgPoolOptions::new().max_connections(cfg.postgres.max_connections).connect_with(opts).await?)
}
