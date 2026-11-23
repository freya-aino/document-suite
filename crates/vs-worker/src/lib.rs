//! The document pipeline as a library. See `SPEC.md`.
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
pub mod pipeline;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use vs_infra::{Config, ObjectBlobStore};

mod error;
pub use error::TaskError;
pub use pipeline::{Pipeline, PipelineConfig, Stage};
use vs_models::{Embedder, ModelRegistry, OpenAiClient, OpenAiEmbedder};
use vs_vault::{ops::Ops, PgCorpusResolver, PgOntologyStore, PgVault};
use vs_vectors::{QdrantConfig, QdrantStore, VectorStore};

/// Refuses a document this long before rendering any of it.
pub const MAX_PAGES: u32 = 5_000;

/// Who this process says it is when it claims a document.
///
/// Host and pid identify it to a person reading the table; the random suffix is what
/// makes it *unique*, because two copies in two containers can share both — and a lease
/// holder that is not unique is a lease that two processes think they hold.
pub fn holder_id() -> String {
    let host = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "host".into());
    format!("{host}/{}/{}", std::process::id(), &uuid::Uuid::now_v7().simple().to_string()[..8])
}

/// Everything a handler needs that does not come out of the config file.
pub struct Deps {
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

/// Builds the pipeline, validating everything it needs up front: a missing model, an
/// unreachable Qdrant or a wrong embedding dimension is a startup failure and not a
/// per-document one.
///
/// All three stages are built, because one process runs all three. That is stricter than
/// the old per-role build: a copy that only rasterized used to need no models and no
/// Qdrant, and now every copy needs both. It is also why the failure is at startup —
/// a service that accepted uploads and could not finish them would be worse.
pub async fn build_pipeline(cfg: &Config, deps: Deps) -> anyhow::Result<Pipeline> {
    let Deps { pool, render_command } = deps;
    let blob: Arc<dyn vs_infra::BlobStore> = Arc::new(ObjectBlobStore::s3(&cfg.blob)?);
    let ops = Arc::new(Ops { pool: pool.clone() });
    let vault: Arc<dyn vs_vault::VaultWriter> = Arc::new(PgVault { pool });
    let models = Models::load(cfg, blob.clone())?;
    let steps = Arc::new(vs_steps::production_registry(models.client.clone()));
    let classify = classify_config(cfg)?;

    let default_visibility = vs_vault::Visibility::parse(cfg.api.default_visibility.trim())
        .ok_or_else(|| anyhow::anyhow!("api.default_visibility {:?}: expected private, department, workspace or organization", cfg.api.default_visibility))?;
    let ingest = ingest::IngestHandler {
        blob: blob.clone(),
        vault: vault.clone(),
        ops: ops.clone(),
        pool: Arc::new(render_pool(cfg, render_command)?),
        scratch_dir: cfg.worker.scratch_dir.clone().unwrap_or_else(std::env::temp_dir),
        bucket: cfg.blob.bucket.clone(),
        render_dpi: cfg.worker.render_dpi,
        // From `[classify]`, not a constant here: the ingest stage renders the thumbnail
        // and the page stage consumes it, so the size comes from one place or the two
        // disagree about what they are passing each other.
        thumbnail_px: classify.thumbnail_long_side_px,
        max_pages: MAX_PAGES,
        default_visibility,
    };

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
    let page = page::PageHandler {
        blob: blob.clone(),
        vault: vault.clone(),
        dags: Arc::new(dags::DagLists::new(ops.clone(), steps.clone(), Duration::from_secs(cfg.stage2.dag_list_refresh_s))),
        ops: ops.clone(),
        steps,
        classify,
        vision,
        bucket: cfg.blob.bucket.clone(),
        render_dpi: cfg.worker.render_dpi,
    };

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
        idf: cfg.qdrant.idf,
    })?);
    // Collections are sized by the embedder, so this must happen after it is built and
    // before the first document: a wrong dimension is unrecoverable once points exist.
    vectors.ensure_ready(embedder.dimensions()).await?;

    let doc_classifier: Option<Arc<dyn vs_classify::DocumentClassifier>> = match &s2.doc_classifier_model {
        Some(model) => {
            models.client.spec(model)?; // fail at startup, not per document
            Some(Arc::new(vs_models::classifier::VlmDocumentClassifier { client: models.client.clone(), model: model.clone(), max_pages: 3 }))
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
    let doc = doc::DocHandler {
        blob,
        vault,
        ontology: Arc::new(PgOntologyStore::new(ops.pool.clone())),
        vectors,
        ops: ops.clone(),
        client: models.client,
        embedder,
        resolver: Arc::new(resolver),
        doc_classifier,
        stage2: s2.clone(),
        max_failed_page_fraction: cfg.worker.max_failed_page_fraction,
    };

    Ok(Pipeline::new(
        ingest,
        page,
        doc,
        ops,
        PipelineConfig {
            max_pages_in_flight: cfg.worker.max_in_flight,
            interactive_share: cfg.worker.interactive_share,
            page_attempts: cfg.worker.page_attempts,
            holder: holder_id(),
            lease_ttl: Duration::from_secs(cfg.worker.lease_ttl_s.max(10)),
        },
    ))
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
