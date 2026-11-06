//! Worker process. See `SPEC.md`.

mod doc;
mod ingest;
mod page;

use clap::{Parser, ValueEnum};

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = vs_infra::Config::load(&args.config)?;
    vs_infra::telemetry::init(&cfg.telemetry);
    tracing::info!(role = ?args.role, "vs-worker starting");

    let role = match args.role {
        RoleArg::Ingest => vs_infra::Role::Ingest,
        RoleArg::Page => vs_infra::Role::Page,
        RoleArg::Doc => vs_infra::Role::Doc,
    };
    run(role, cfg).await
}

/// Common loop (SPEC.md "Common loop"): connect, build registries, pull tasks from both lanes,
/// dispatch to the role handler, ack/nak/dead-letter.
async fn run(role: vs_infra::Role, cfg: vs_infra::Config) -> anyhow::Result<()> {
    let _ = (role, cfg.worker.max_in_flight, cfg.worker.interactive_share);
    // let bus = vs_infra::NatsBus::connect(&cfg.bus.nats_url).await?;
    // let steps = page::build_step_registry(client.clone());
    // let router = load_active_router(&pool, &steps).await?;
    // let mut interactive = bus.subscribe(role, Lane::Interactive).await?;
    // let mut batch = bus.subscribe(role, Lane::Batch).await?;
    // select! over both streams with a semaphore of max_in_flight, reserving interactive_share.
    todo!("wire the common loop")
}
