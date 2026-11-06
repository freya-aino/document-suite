use tracing_subscriber::{fmt, EnvFilter};

use crate::config::TelemetryConfig;

/// Initialises logging. OTLP export and the Prometheus endpoint are wired here once the
/// exporter crates are chosen (see SPEC.md).
pub fn init(cfg: &TelemetryConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if cfg.json_logs {
        fmt().with_env_filter(filter).json().init();
    } else {
        // No colour codes when logging to a file or pipe.
        let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
        fmt().with_env_filter(filter).with_ansi(ansi).init();
    }
    if cfg.otlp_endpoint.is_some() || cfg.metrics_addr.is_some() {
        tracing::warn!("OTLP / metrics exporters not wired yet");
    }
}
