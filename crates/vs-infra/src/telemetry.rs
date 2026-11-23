//! Logs, metrics and traces for every binary in the workspace.
//!
//! One entry point so a worker and `vs-api` are observable the same way. The important
//! asymmetry is where the Prometheus series are *served* from: `vs-api` already runs an
//! HTTP server and renders them on its own `/metrics` route, while a worker has no server
//! at all and needs this module to bind a listener for it. Both get the same recorder, and
//! there is exactly one of it, because `metrics::set_global_recorder` can only be called
//! once per process.

use std::net::SocketAddr;
use std::time::Duration;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

use crate::config::TelemetryConfig;

/// Histogram buckets, in seconds, for everything this workspace times.
///
/// The exporter's defaults top out around 10 s, which would put a page render, a VLM call
/// and a whole Stage 2 document in one overflow bucket — the three durations anyone
/// actually wants a quantile for. These span 5 ms to 10 minutes instead.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// How long `shutdown` waits for buffered spans to reach the collector.
///
/// Shorter than the worker's shutdown grace, because a worker that cannot reach its
/// collector must still exit before the orchestrator kills it. Losing the last batch of
/// spans is an acceptable price for that; blocking the drain is not.
const TRACE_FLUSH: Duration = Duration::from_secs(3);

/// What `init` hands back to its caller.
pub struct Telemetry {
    /// The live registry, for a binary that renders `/metrics` itself.
    pub metrics: PrometheusHandle,
    /// Held so the batch span processor stays alive for the length of the process, and so
    /// `shutdown` has something to flush. `None` when `otlp_endpoint` is unset.
    traces: Option<SdkTracerProvider>,
}

impl Telemetry {
    /// Flushes buffered spans. Call it before exiting, or the last batch — which is the
    /// one covering whatever made you shut down — never leaves the process.
    pub fn shutdown(&self) {
        if let Some(p) = &self.traces {
            if let Err(e) = p.shutdown_with_timeout(TRACE_FLUSH) {
                tracing::warn!(error = %e, "could not flush traces on shutdown");
            }
        }
    }
}

/// Initialises logging and installs the process-wide metrics recorder.
///
/// `service` becomes a global label on every series, so a Prometheus scraping the three
/// worker roles and the API can tell whose counter it is reading. Must be called from
/// inside a Tokio runtime, and exactly once: a second call fails on the global recorder
/// rather than quietly producing a second set of series.
pub fn init(cfg: &TelemetryConfig, service: &str) -> anyhow::Result<Telemetry> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let logs = if cfg.json_logs {
        fmt::layer().json().boxed()
    } else {
        // No colour codes when logging to a file or pipe.
        fmt::layer().with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())).boxed()
    };

    // Spans go to the collector *and* the logs. The exporter is built before the
    // subscriber so that an unreachable or malformed endpoint is a startup error rather
    // than a silent fallback to logs-only.
    let traces = match &cfg.otlp_endpoint {
        Some(endpoint) => Some(trace_provider(endpoint, service, cfg.sample_ratio)?),
        None => {
            tracing::debug!("telemetry.otlp_endpoint unset; spans go to the logs only");
            None
        }
    };
    let otel = traces.as_ref().map(|p| {
        use opentelemetry::trace::TracerProvider as _;
        tracing_opentelemetry::layer().with_tracer(p.tracer("vs"))
    });

    // `try_init`, not `init`: a second call is a programming error, and it should come
    // back as one rather than panicking inside a library.
    tracing_subscriber::registry()
        .with(filter)
        .with(logs)
        .with(otel)
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing subscriber already installed: {e}"))?;

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

    Ok(Telemetry { metrics, traces })
}

/// The sampler for a given ratio.
///
/// Parent-based around a ratio sampler, which is the combination that matters: the ratio
/// decides whether a *trace* is recorded, at the span that starts it, and every span
/// below it then follows its parent's decision. Sampling each span independently would
/// export fragments of traces — a Stage 2 span with no document span above it — which is
/// worse than exporting fewer whole ones.
///
/// 1.0 (the default) is `AlwaysOn`, not a ratio sampler with probability 1, so the
/// common case carries no sampling decision at all. A ratio outside 0.0..=1.0 is clamped
/// rather than refused: a typo in a telemetry setting should not stop the service, and
/// the clamp is logged.
fn sampler(ratio: f64) -> opentelemetry_sdk::trace::Sampler {
    use opentelemetry_sdk::trace::Sampler;
    let r = if ratio.is_nan() { 1.0 } else { ratio.clamp(0.0, 1.0) };
    if r != ratio {
        tracing::warn!(configured = ratio, used = r, "telemetry.sample_ratio is outside 0.0..=1.0; clamped");
    }
    match r {
        r if r >= 1.0 => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        r if r <= 0.0 => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        r => Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(r))),
    }
}

/// Batch-exports spans over OTLP HTTP/protobuf.
///
/// HTTP rather than gRPC because `reqwest` with rustls is already a dependency of this
/// crate and `tonic` is not, and every collector speaks both. The consequence for a
/// deployment is the port: OTLP/HTTP is 4318, not gRPC's 4317.
fn trace_provider(endpoint: &str, service: &str, sample_ratio: f64) -> anyhow::Result<SdkTracerProvider> {
    use opentelemetry_otlp::WithExportConfig as _;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
        .with_timeout(TRACE_FLUSH)
        .build()?;
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler(sample_ratio))
        // The same name the Prometheus series carry, so a trace and a counter from the
        // same process are joinable.
        .with_resource(opentelemetry_sdk::Resource::builder().with_service_name(service.to_string()).build())
        .build())
}
