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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    vs_vault::MIGRATOR.run(&pool).await?; // advisory-locked; safe with many workers
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
