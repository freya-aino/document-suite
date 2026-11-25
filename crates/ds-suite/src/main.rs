//! The service: the HTTP API and the document pipeline in one process.
//!
//! One binary, one process, one deployable unit, and now one path through the work. There
//! used to be a task bus between the stages and three roles to deploy; the per-page DAG
//! was always executed in-process, so the queue's only unique contribution was spreading
//! one document's pages across machines — at the cost of a service to run and a
//! redelivery protocol to maintain. `ds_worker::Pipeline` is what replaced it: ingest,
//! then the pages concurrently, then Stage 2, as function calls.
//!
//! What that gives up, stated plainly: a single large document is now processed by one
//! copy rather than spread over the fleet. What it does *not* give up is recovery — each
//! document is leased, and `resume` claims the ones a crashed copy left behind.
//!
//! What must exist around it is in `deploy/README.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{watch, Semaphore};

use ds_api::auth::Auth;
use ds_api::Dispatch;
use ds_core::{JobSpec, RunId};
use ds_infra::{Config, ObjectBlobStore};
use ds_worker::{build_pipeline, connect_pg, holder_id, self_render_command, Deps, Pipeline};

mod resume;

#[derive(Parser)]
#[command(about = "The document-suite service: HTTP API and document pipeline in one process")]
struct Args {
    #[arg(long, env = "DS_CONFIG", default_value = "/etc/ds/worker.toml")]
    config: std::path::PathBuf,
    /// Where the HTTP API listens.
    #[arg(long, env = "DS_API_ADDR", default_value = "0.0.0.0:8080")]
    addr: String,
    /// Seconds to wait for documents still being processed on SIGTERM.
    #[arg(long, env = "DS_SHUTDOWN_GRACE", default_value_t = 60)]
    shutdown_grace: u64,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Apply database migrations (deploy step; needs the schema owner's credentials,
    /// not the runtime role's).
    Migrate {
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
    },
}

fn main() -> anyhow::Result<()> {
    // `ds-suite render-child`: the render process the ingest stage's pool spawns, which is
    // this binary re-executed (`self_render_command`). It speaks a binary protocol on
    // stdout, so it must not start the runtime or logging.
    if std::env::args().nth(1).as_deref() == Some("render-child") {
        return Ok(ds_ingest::child::serve(std::io::stdin().lock(), std::io::stdout().lock())?);
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(Cmd::Migrate { database_url }) = &args.cmd {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        ds_vault::MIGRATOR.run(&pool).await?;
        ds_vault::check_migrations(&pool).await?;
        println!("migrations applied");
        return Ok(());
    }

    let cfg = Config::load(&args.config)?;
    // Read before anything is built: there is no configuration in which these routes
    // answer without verifying tokens.
    let auth_cfg = cfg.auth.clone().ok_or_else(|| {
        anyhow::anyhow!("ds-suite needs an [auth] section in {}: issuer and audience of the Keycloak realm", args.config.display())
    })?;

    let telemetry = ds_infra::telemetry::init(&cfg.telemetry, "ds-suite")?;
    tracing::info!(config = %args.config.display(), "ds-suite starting");

    let pool = connect_pg(&cfg).await?;
    ds_vault::check_migrations(&pool).await?; // migrations are a deploy step (`ds-suite migrate`)

    // Every stage is built here, so a missing model or an unreachable Qdrant fails now
    // rather than on the first document that reaches that stage.
    let pipeline = Arc::new(build_pipeline(&cfg, Deps { pool: pool.clone(), render_command: self_render_command(&cfg)? }).await?);
    let dispatch = Arc::new(Documents::new(pipeline, cfg.worker.max_documents));

    let auth = Auth::discover(auth_cfg).await?;
    auth.spawn_refresher();
    let state = api_state(&cfg, pool, dispatch.clone(), auth.clone(), telemetry.metrics.clone())?;
    // Revocation: a token stays good for its `exp` unless something says otherwise, and
    // this is that something. Given the same `Ops` the routes use, so a revocation
    // recorded here applies to this copy at once and to the others on their next refresh.
    auth.with_revocations(state.ops.clone());
    auth.spawn_revocation_refresher(Duration::from_secs(cfg.api.revocation_refresh_s));
    ds_api::spawn_idempotency_purge(state.ops.clone());
    // Retention for the one place personal data lives (`docs/SPEC.md` §5.6).
    ds_api::scope::spawn_retention_sweep(state.ops.clone(), cfg.api.job_retention_days);
    // Retention for the object store: rendered pages, results, crops and the scratch
    // images handed to models. `[blob].retention` is what answers open question 4.
    spawn_blob_retention(&cfg)?;
    // Recovery for documents a crashed copy abandoned. See `resume`.
    resume::spawn(
        state.ops.clone(),
        dispatch.clone(),
        resume::ResumeConfig {
            holder: holder_id(),
            lease_ttl: Duration::from_secs(cfg.worker.lease_ttl_s.max(10)),
            interval: Duration::from_secs(cfg.worker.resume_interval_s),
            batch: cfg.worker.resume_batch,
            max_resumes: cfg.worker.max_resumes,
            max_documents: cfg.worker.max_documents.max(1) as u64,
        },
    );

    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown requested");
        let _ = stop_tx.send(true);
    });

    // The limits this copy is working under, as gauges, because a reader of
    // `worker_stage_in_flight` needs the denominator to make a fraction of it — which is
    // what an autoscaler scales on (`deploy/firecracker/fleet.sh`).
    metrics::gauge!("suite_max_documents").set(cfg.worker.max_documents as f64);
    metrics::gauge!("suite_max_pages_in_flight").set(cfg.worker.max_in_flight as f64);

    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    tracing::info!(addr = %args.addr, max_documents = cfg.worker.max_documents, max_pages = cfg.worker.max_in_flight, "http api listening");
    let serving = axum::serve(listener, ds_api::router(state)).with_graceful_shutdown(async move {
        let _ = stop_rx.wait_for(|stopping| *stopping).await;
    });
    serving.await?;
    tracing::info!("http api stopped; draining documents");

    // Nothing will redeliver these, so they are worth waiting for. Past the grace they
    // are abandoned mid-document and the job rows stay unfinished — a caller re-POST
    // resumes them.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.shutdown_grace);
    while dispatch.in_flight() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let left = dispatch.in_flight();
    if left > 0 {
        tracing::warn!(documents = left, "grace elapsed with documents still in flight; they will need re-uploading");
    }
    telemetry.shutdown();
    Ok(())
}

/// Deletes derived objects past their retention, every `blob.retention.sweep_interval_h`.
///
/// In every copy, which is safe because the sweep is idempotent — it deletes what is
/// older than a cutoff, so two copies racing delete the same objects and the second finds
/// them gone. The alternative, electing one sweeper, would need a lock for a job whose
/// worst case is a wasted LIST.
fn spawn_blob_retention(cfg: &Config) -> anyhow::Result<()> {
    let retention = cfg.blob.retention.clone();
    if retention.sweep_interval_h == 0 {
        tracing::info!("blob.retention.sweep_interval_h = 0: nothing deletes derived objects");
        return Ok(());
    }
    let blob: Arc<dyn ds_infra::BlobStore> = Arc::new(ObjectBlobStore::s3(&cfg.blob)?);
    let period = Duration::from_secs(retention.sweep_interval_h * 3600);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        // Not immediately: a copy coming up has better things to do than a full LIST of
        // every prefix, and nothing is urgent about an object a day past its retention.
        tick.tick().await;
        loop {
            tick.tick().await;
            let swept = ds_infra::blob::sweep_retention(blob.as_ref(), &retention).await;
            let total: u64 = swept.iter().map(|(_, n)| *n).sum();
            if total > 0 {
                metrics::counter!("suite_blob_objects_pruned_total").increment(total);
            }
        }
    });
    Ok(())
}

/// Runs accepted documents on this process, a bounded number at a time.
///
/// `dispatch` returns as soon as the document is accepted — the response is a `202` and a
/// job id — and the work then waits for a slot in the spawned task rather than holding the
/// request open. What stops that waiting set from growing without limit is the API's own
/// admission check, which reads [`Documents::in_flight`].
///
/// The document is **leased before it is queued**, not when it starts. An unleased job is
/// what an abandoned one looks like to the resume sweep, and a document waiting for a slot
/// has no lease unless something takes one: under load that wait outlasts
/// `worker.lease_ttl_s`, so another copy claimed the job, `resumes` went up, and after
/// `worker.max_resumes` of that a healthy document was failed without anyone having
/// started it. See [`ds_worker::Pipeline::lease`].
struct Documents {
    pipeline: Arc<Pipeline>,
    slots: Arc<Semaphore>,
    in_flight: Arc<AtomicU64>,
}

impl Documents {
    fn new(pipeline: Arc<Pipeline>, max: usize) -> Self {
        Self { pipeline, slots: Arc::new(Semaphore::new(max.max(1))), in_flight: Arc::new(AtomicU64::new(0)) }
    }
}

/// Counts a document as in flight for as long as its task lives, however that task ends.
///
/// A guard rather than a pair of calls: the counter is what admission control and the
/// resume sweep both read, and a decrement skipped by an unwinding task would leave a
/// copy that reads as permanently busy — rejecting interactive uploads and claiming no
/// abandoned work — until it is restarted.
struct Counted(Arc<AtomicU64>);

impl Counted {
    fn one(counter: &Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl Dispatch for Documents {
    async fn dispatch(&self, job_id: uuid::Uuid, run_id: RunId, spec: JobSpec) -> anyhow::Result<()> {
        // Before the slot, and before this returns: a document nobody has leased is a
        // document the sweep is entitled to take away, and the caller needs to hear that
        // this copy is not the one carrying it.
        let lease = match self.pipeline.lease(job_id).await {
            Ok(Some(lease)) => lease,
            Ok(None) => anyhow::bail!("another copy holds the lease on this document"),
            Err(e) => return Err(e.error),
        };
        let counted = Counted::one(&self.in_flight);
        let (pipeline, slots) = (self.pipeline.clone(), self.slots.clone());
        tokio::spawn(async move {
            // Held across the wait for a slot and across the work itself, then released.
            let (_lease, _counted) = (lease, counted);
            let _slot = slots.acquire_owned().await.expect("document semaphore is never closed");
            let span = tracing::info_span!("document", %job_id, tenant = spec.tenant.as_str());
            let result = tracing::Instrument::instrument(pipeline.run(job_id, run_id, &spec), span).await;
            if let Err(e) = result {
                // Already recorded on the job row by the pipeline; logged here because
                // nothing else is watching this task.
                tracing::error!(%job_id, permanent = e.permanent, error = %format!("{:#}", e.error), "document failed");
            }
        });
        Ok(())
    }

    fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }
}

/// The API's state, over the connections the pipeline in this process shares.
fn api_state(
    cfg: &Config,
    pool: sqlx::PgPool,
    dispatch: Arc<dyn Dispatch>,
    auth: Arc<Auth>,
    metrics: metrics_exporter_prometheus::PrometheusHandle,
) -> anyhow::Result<ds_api::AppState> {
    // DAG-list uploads are validated against the production step registry, which needs
    // the model registry; without it that one route answers 503 and the rest still serve.
    let steps = match ds_models::ModelRegistry::load(&cfg.models.registry) {
        Ok(reg) => Some(Arc::new(ds_steps::production_registry(Arc::new(ds_models::OpenAiClient::new(reg))))),
        Err(e) => {
            tracing::error!(path = %cfg.models.registry.display(), error = %e, "model registry not loaded; DAG-list uploads disabled");
            None
        }
    };
    // Read scoping needs a valid default, and a typo in it must not come up as a
    // service that silently shares every upload with the whole organization.
    let visibility = ds_vault::Visibility::parse(cfg.api.default_visibility.trim()).ok_or_else(|| {
        anyhow::anyhow!("api.default_visibility {:?}: expected private, department, workspace or organization", cfg.api.default_visibility)
    })?;
    tracing::info!(
        default_visibility = visibility.as_str(),
        admin_reads_all = cfg.api.admin_reads_all,
        rate_per_s = cfg.api.tenant_rate_per_s,
        "read scoping"
    );
    Ok(ds_api::AppState {
        ops: Arc::new(ds_vault::ops::Ops { pool }),
        auth,
        dispatch,
        blob: Arc::new(ObjectBlobStore::s3(&cfg.blob)?),
        bucket: cfg.blob.bucket.clone(),
        api: cfg.api.clone(),
        limiter: Arc::new(ds_api::limit::RateLimiter::new(cfg.api.tenant_rate_per_s, cfg.api.tenant_rate_burst)),
        steps,
        metrics,
    })
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
