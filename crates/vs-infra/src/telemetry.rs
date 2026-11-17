//! Logs, metrics and traces for every binary in the workspace.
//!
//! One entry point so a worker and `vs-api` are observable the same way. The important
//! asymmetry is where the Prometheus series are *served* from: `vs-api` already runs an
//! HTTP server and renders them on its own `/metrics` route, while a worker has no server
//! at all and needs this module to bind a listener for it. Both get the same recorder, and
//! there is exactly one of it, because `metrics::set_global_recorder` can only be called
//! once per process.

use std::net::SocketAddr;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing_subscriber::{fmt, EnvFilter};

use crate::config::TelemetryConfig;

/// Histogram buckets, in seconds, for everything this workspace times.
///
/// The exporter's defaults top out around 10 s, which would put a page render, a VLM call
/// and a whole Stage 2 document in one overflow bucket — the three durations anyone
/// actually wants a quantile for. These span 5 ms to 10 minutes instead.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// What `init` hands back to its caller.
pub struct Telemetry {
    /// The live registry, for a binary that renders `/metrics` itself.
    pub metrics: PrometheusHandle,
}

/// Initialises logging and installs the process-wide metrics recorder.
///
/// `service` becomes a global label on every series, so a Prometheus scraping the three
/// worker roles and the API can tell whose counter it is reading. Must be called from
/// inside a Tokio runtime, and exactly once: a second call fails on the global recorder
/// rather than quietly producing a second set of series.
pub fn init(cfg: &TelemetryConfig, service: &str) -> anyhow::Result<Telemetry> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init`, not `init`: a second call is a programming error, and it should come
    // back as one rather than panicking inside a library.
    let logs = if cfg.json_logs {
        fmt().with_env_filter(filter).json().try_init()
    } else {
        // No colour codes when logging to a file or pipe.
        let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
        fmt().with_env_filter(filter).with_ansi(ansi).try_init()
    };
    logs.map_err(|e| anyhow::anyhow!("tracing subscriber already installed: {e}"))?;

    let builder = PrometheusBuilder::new().set_buckets(DURATION_BUCKETS)?.add_global_label("service", service);
    let metrics = match &cfg.metrics_addr {
        Some(addr) => {
            let addr: SocketAddr = addr.parse().map_err(|e| anyhow::anyhow!("telemetry.metrics_addr {addr:?}: {e}"))?;
            let (recorder, exporter) = builder.with_http_listener(addr).build()?;
            let handle = recorder.handle();
            metrics::set_global_recorder(recorder).map_err(|e| anyhow::anyhow!("metrics recorder already installed: {e}"))?;
            tokio::spawn(exporter);
            tracing::info!(%addr, "serving Prometheus metrics");
            handle
        }
        None => {
            // Still install a recorder. Without one every `counter!` in the workspace is
            // a silent no-op, and a binary that serves `/metrics` from its own router
            // needs the handle regardless of this setting.
            let recorder = builder.build_recorder();
            let handle = recorder.handle();
            metrics::set_global_recorder(recorder).map_err(|e| anyhow::anyhow!("metrics recorder already installed: {e}"))?;
            handle
        }
    };

    if cfg.otlp_endpoint.is_some() {
        tracing::warn!("OTLP trace exporter not wired yet");
    }
    Ok(Telemetry { metrics })
}
