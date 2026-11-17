//! The service: the HTTP API and the worker roles in one process.
//!
//! One binary, one process, one deployable unit. The suite used to be two — `vs-api` and
//! `vs-worker`, the latter run three times — which meant a deployment embedding this
//! project had to know the suite's internal shape to run it at all. It does not need to:
//! the roles are a concurrency decision, not a packaging one.
//!
//! So this process serves the routes in [`vs_api`] *and* runs the three worker loops in
//! [`vs_worker`], over connections they share: one Postgres pool, one NATS connection,
//! one metrics recorder. Scaling out is more copies of this service; `--roles` narrows a
//! copy to part of the work when a deployment wants the halves scaled separately
//! (goal G8), which is a choice it can make rather than one made for it.
//!
//! What must exist around it is in `deploy/README.md`.

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use sqlx::postgres::PgPoolOptions;
use tokio::sync::watch;
use tokio::task::JoinSet;

use vs_api::auth::Auth;
use vs_infra::queue::{NatsBus, Role, TaskBus};
use vs_infra::{Config, ObjectBlobStore};
use vs_worker::{build_handler, connect_pg, runtime, self_render_command, Deps};

/// What a copy of the service does. All four by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum RoleArg {
    /// Serve the HTTP API.
    Api,
    /// Rasterize, extract the text layer, measure page signals.
    Ingest,
    /// Stage 1, per page.
    Page,
    /// Stage 2, per document.
    Doc,
}

impl RoleArg {
    fn as_str(self) -> &'static str {
        match self {
            RoleArg::Api => "api",
            RoleArg::Ingest => "ingest",
            RoleArg::Page => "page",
            RoleArg::Doc => "doc",
        }
    }

    /// The worker role this is, or `None` for the API.
    fn worker(self) -> Option<Role> {
        match self {
            RoleArg::Api => None,
            RoleArg::Ingest => Some(Role::Ingest),
            RoleArg::Page => Some(Role::Page),
            RoleArg::Doc => Some(Role::Doc),
        }
    }
}

#[derive(Parser)]
#[command(about = "The visual-suite service: HTTP API and worker roles in one process")]
struct Args {
    /// What this copy runs. The default is everything, which is the point: one service.
    #[arg(long, env = "VS_ROLES", value_enum, value_delimiter = ',', default_value = "api,ingest,page,doc")]
    roles: Vec<RoleArg>,
    #[arg(long, env = "VS_CONFIG", default_value = "/etc/vs/worker.toml")]
    config: std::path::PathBuf,
    /// Where the HTTP API listens. Ignored without the `api` role.
    #[arg(long, env = "VS_API_ADDR", default_value = "0.0.0.0:8080")]
    addr: String,
    /// Seconds to wait for in-flight tasks on SIGTERM.
    #[arg(long, env = "VS_SHUTDOWN_GRACE", default_value_t = 60)]
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
    // `vs-suite render-child`: the render process the ingest role's pool spawns, which is
    // this binary re-executed (`self_render_command`). It speaks a binary protocol on
    // stdout, so it must not start the runtime or logging.
    if std::env::args().nth(1).as_deref() == Some("render-child") {
        return Ok(vs_ingest::child::serve(std::io::stdin().lock(), std::io::stdout().lock())?);
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(Cmd::Migrate { database_url }) = &args.cmd {
        let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
        vs_vault::MIGRATOR.run(&pool).await?;
        vs_vault::check_migrations(&pool).await?;
        println!("migrations applied");
        return Ok(());
    }

    let roles = roles(&args.roles);
    let cfg = Config::load(&args.config)?;
    let serve_api = roles.contains(&RoleArg::Api);

    // Read before anything is built, and only when this copy serves the API: there is no
    // configuration in which these routes answer without verifying tokens, and no reason
    // to demand a realm of a copy that serves none.
    let auth_cfg = if serve_api {
        Some(cfg.auth.clone().ok_or_else(|| {
            anyhow::anyhow!("the api role needs an [auth] section in {}: issuer and audience of the Keycloak realm", args.config.display())
        })?)
    } else {
        None
    };

    // One recorder and one tracer per process, whatever this copy runs. The service label
    // says which roles, so two copies with different `--roles` stay apart in the metrics.
    let telemetry = vs_infra::telemetry::init(&cfg.telemetry, &service_name(&roles))?;
    tracing::info!(roles = %label(&roles), config = %args.config.display(), "vs-suite starting");

    // Shared by every role in this process: one pool, one bus connection.
    let pool = connect_pg(&cfg).await?;
    vs_vault::check_migrations(&pool).await?; // migrations are a deploy step (`vs-suite migrate`)
    let nats = NatsBus::connect(&cfg.bus.nats_url).await?;
    nats.ensure_topology().await?;
    let bus: Arc<dyn TaskBus> = Arc::new(nats);

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown requested");
        let _ = stop_tx.send(true);
    });

    let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();

    for role in roles.iter().filter_map(|r| r.worker()) {
        // A fresh `Deps` per role: `build_handler` consumes it, and the render command is
        // cheap to resolve again. The pool and the bus are the shared ones.
        let deps = Deps { bus: bus.clone(), pool: pool.clone(), render_command: self_render_command(&cfg)? };
        let handler = build_handler(&cfg, role, deps).await?;
        let loop_cfg = runtime::LoopConfig {
            max_in_flight: cfg.worker.max_in_flight,
            interactive_share: cfg.worker.interactive_share,
            shutdown_grace: Duration::from_secs(args.shutdown_grace),
        };
        let (bus, stop) = (bus.clone(), stop_rx.clone());
        tracing::info!(role = role.as_str(), max_in_flight = cfg.worker.max_in_flight, "role started");
        tasks.spawn(async move { runtime::run_loop(role, bus, handler, loop_cfg, stop).await });
    }

    if let Some(auth_cfg) = auth_cfg {
        let auth = Auth::discover(auth_cfg).await?;
        auth.spawn_refresher();
        let state = api_state(&cfg, pool.clone(), bus.clone(), auth, telemetry.metrics.clone())?;
        vs_api::spawn_idempotency_purge(state.ops.clone());

        let listener = tokio::net::TcpListener::bind(&args.addr).await?;
        tracing::info!(addr = %args.addr, "http api listening");
        let app = vs_api::router(state);
        let mut stop = stop_rx.clone();
        tasks.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    // The same signal the worker loops drain on, so the process stops
                    // accepting requests and finishes its in-flight tasks together.
                    let _ = stop.wait_for(|stopping| *stopping).await;
                })
                .await?;
            tracing::info!("http api stopped");
            Ok(())
        });
    }

    // The first failure wins and the rest are abandoned: a role that cannot run is not a
    // degraded service, it is a misconfigured one, and a deployment should see it fail.
    let mut result = Ok(());
    while let Some(joined) = tasks.join_next().await {
        let outcome = joined.map_err(anyhow::Error::from).and_then(|r| r);
        if let Err(e) = outcome {
            tracing::error!(error = %e, "a role stopped with an error");
            if result.is_ok() {
                result = Err(e);
            }
        }
    }
    // After the drain, so the spans of the tasks that finished during it are included.
    telemetry.shutdown();
    result
}

/// The API's state, over the connections the worker roles in this process share.
fn api_state(
    cfg: &Config,
    pool: sqlx::PgPool,
    bus: Arc<dyn TaskBus>,
    auth: Arc<Auth>,
    metrics: metrics_exporter_prometheus::PrometheusHandle,
) -> anyhow::Result<vs_api::AppState> {
    // DAG-list uploads are validated against the production step registry, which needs
    // the model registry; without it that one route answers 503 and the rest still serve.
    let steps = match vs_models::ModelRegistry::load(&cfg.models.registry) {
        Ok(reg) => Some(Arc::new(vs_steps::production_registry(Arc::new(vs_models::OpenAiClient::new(reg))))),
        Err(e) => {
            tracing::error!(path = %cfg.models.registry.display(), error = %e, "model registry not loaded; DAG-list uploads disabled");
            None
        }
    };
    Ok(vs_api::AppState {
        ops: Arc::new(vs_vault::ops::Ops { pool }),
        auth,
        bus,
        blob: Arc::new(ObjectBlobStore::s3(&cfg.blob)?),
        bucket: cfg.blob.bucket.clone(),
        api: cfg.api.clone(),
        steps,
        metrics,
    })
}

/// Sorted and deduplicated, so `--roles doc,api,doc` is `api,doc` and the service label
/// of two copies running the same work is the same string.
fn roles(args: &[RoleArg]) -> Vec<RoleArg> {
    let mut r = args.to_vec();
    r.sort_unstable();
    r.dedup();
    r
}

fn label(roles: &[RoleArg]) -> String {
    roles.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(",")
}

/// `vs-suite` when this copy is the whole service, `vs-suite-<roles>` when it is a part.
///
/// The label joins a process's metrics and its traces (§5.5), so a copy running only the
/// `page` role must not report under the same name as one running everything.
fn service_name(roles: &[RoleArg]) -> String {
    if roles.len() == 4 {
        "vs-suite".into()
    } else {
        format!("vs-suite-{}", roles.iter().map(|r| r.as_str()).collect::<Vec<_>>().join("-"))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_deduplicated_and_ordered() {
        assert_eq!(roles(&[RoleArg::Doc, RoleArg::Api, RoleArg::Doc]), vec![RoleArg::Api, RoleArg::Doc]);
        assert_eq!(label(&roles(&[RoleArg::Page, RoleArg::Ingest])), "ingest,page");
    }

    #[test]
    fn the_service_label_says_which_part_of_the_service_this_is() {
        // The whole service reports under one name; a part says which part, because the
        // label is what joins a process's metrics to its traces.
        assert_eq!(service_name(&roles(&[RoleArg::Api, RoleArg::Ingest, RoleArg::Page, RoleArg::Doc])), "vs-suite");
        assert_eq!(service_name(&roles(&[RoleArg::Page])), "vs-suite-page");
        assert_eq!(service_name(&roles(&[RoleArg::Doc, RoleArg::Api])), "vs-suite-api-doc");
        // Duplicates must not look like the whole service.
        assert_eq!(service_name(&roles(&[RoleArg::Api, RoleArg::Api, RoleArg::Api, RoleArg::Api])), "vs-suite-api");
    }

    #[test]
    fn every_role_maps_to_itself() {
        use clap::ValueEnum;
        for role in RoleArg::value_variants() {
            assert_eq!(RoleArg::from_str(role.as_str(), true).expect("round trip"), *role);
        }
        assert_eq!(RoleArg::Api.worker(), None, "the api role is not a queue consumer");
        for role in [RoleArg::Ingest, RoleArg::Page, RoleArg::Doc] {
            assert_eq!(role.worker().map(|r| r.as_str()), Some(role.as_str()));
        }
    }
}
