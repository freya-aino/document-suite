//! Worker process. See `SPEC.md`.

mod doc;
mod ingest;
mod page;
mod runtime;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::watch;

use vs_infra::queue::{NatsBus, Role, TaskBus};
use vs_infra::{Config, ObjectBlobStore};
use vs_vault::{ops::Ops, PgVault};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RoleArg {
    Ingest,
    Page,
    Doc,
}

#[derive(Parser)]
struct Args {
    #[arg(long, env = "VS_ROLE", value_enum)]
    role: RoleArg,
    #[arg(long, env = "VS_CONFIG", default_value = "/etc/vs/worker.toml")]
    config: std::path::PathBuf,
    /// Seconds to wait for in-flight tasks on SIGTERM.
    #[arg(long, env = "VS_SHUTDOWN_GRACE", default_value_t = 60)]
    shutdown_grace: u64,
}

/// Classifier thumbnail size (vs-classify default).
const THUMBNAIL_PX: u32 = 768;
const MAX_PAGES: u32 = 5_000;

fn main() -> anyhow::Result<()> {
    // `vs-worker render-child`: render process spawned by the ingest role's pool. It
    // speaks a binary protocol on stdout, so it must not start the runtime or logging.
    if std::env::args().nth(1).as_deref() == Some("render-child") {
        return Ok(vs_ingest::child::serve(std::io::stdin().lock(), std::io::stdout().lock())?);
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run_worker())
}

async fn run_worker() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    vs_infra::telemetry::init(&cfg.telemetry);
    let role = match args.role {
        RoleArg::Ingest => Role::Ingest,
        RoleArg::Page => Role::Page,
        RoleArg::Doc => Role::Doc,
    };
    tracing::info!(role = role.as_str(), "vs-worker starting");

    let pool = connect_pg(&cfg).await?;
    vs_vault::check_migrations(&pool).await?; // migrations are a deploy step (`vs-api migrate`)
    let nats = NatsBus::connect(&cfg.bus.nats_url).await?;
    nats.ensure_topology().await?;
    let bus: Arc<dyn TaskBus> = Arc::new(nats);
    let blob = Arc::new(ObjectBlobStore::s3(&cfg.blob)?);
    let ops = Arc::new(Ops { pool: pool.clone() });
    let vault = Arc::new(PgVault { pool });

    let handler: Arc<dyn runtime::Handler> = match role {
        Role::Ingest => Arc::new(ingest::IngestHandler {
            blob,
            bus: bus.clone(),
            vault,
            ops,
            pool: Arc::new(render_pool(&cfg)?),
            scratch_dir: cfg.worker.scratch_dir.clone().unwrap_or_else(std::env::temp_dir),
            bucket: cfg.blob.bucket.clone(),
            render_dpi: cfg.worker.render_dpi,
            thumbnail_px: THUMBNAIL_PX,
            max_pages: MAX_PAGES,
        }),
        // Not started rather than consuming tasks it cannot handle.
        Role::Page | Role::Doc => anyhow::bail!("role {} is not implemented yet", role.as_str()),
    };

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown requested");
        let _ = stop_tx.send(true);
    });
    let loop_cfg = runtime::LoopConfig {
        max_in_flight: cfg.worker.max_in_flight,
        interactive_share: cfg.worker.interactive_share,
        shutdown_grace: Duration::from_secs(args.shutdown_grace),
    };
    runtime::run_loop(role, bus, handler, loop_cfg, stop_rx).await
}

/// Render children are this binary with `render-child`.
fn render_pool(cfg: &Config) -> anyhow::Result<vs_ingest::RenderPool> {
    let w = &cfg.worker;
    let size = w.render_processes.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2));
    let env = w
        .render_mem_limit_mb
        .map(|mb| vec![(vs_ingest::child::MEM_LIMIT_ENV.to_string(), mb.to_string())])
        .unwrap_or_default();
    let cmd = vs_ingest::ChildCommand { program: std::env::current_exe()?, args: vec!["render-child".into()], env };
    let timeout = Duration::from_secs(w.page_render_timeout_s);
    tracing::info!(size, ?timeout, "render pool");
    Ok(vs_ingest::RenderPool::new(cmd, vs_ingest::PoolConfig { size, page_timeout: timeout, open_timeout: timeout }))
}

async fn connect_pg(cfg: &Config) -> anyhow::Result<sqlx::PgPool> {
    let mut opts = PgConnectOptions::from_str(&cfg.postgres.url)?;
    if let Some(pw) = &cfg.postgres.password {
        opts = opts.password(pw);
    }
    Ok(PgPoolOptions::new().max_connections(cfg.postgres.max_connections).connect_with(opts).await?)
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
