use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bus: BusConfig,
    pub blob: BlobConfig,
    pub postgres: PostgresConfig,
    pub helix: HelixConfig,
    pub models: ModelsConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Passed through to `vs-classify` (`ClassifyConfig`).
    #[serde(default = "empty_table")]
    pub classify: toml::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BusConfig {
    pub nats_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BlobConfig {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgresConfig {
    /// Without password; password comes from `VS__POSTGRES__PASSWORD`.
    pub url: String,
    /// Set from the environment only.
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_pool")]
    pub max_connections: u32,
}

fn empty_table() -> toml::Value {
    toml::Value::Table(Default::default())
}

fn default_pool() -> u32 {
    16
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixConfig {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsConfig {
    /// Path to the model registry TOML (see config/models.example.toml).
    pub registry: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// Max concurrent tasks per worker process.
    pub max_in_flight: usize,
    /// Share of in-flight slots reserved for the interactive lane.
    pub interactive_share: f32,
    /// Render DPI for page images.
    pub render_dpi: u32,
    /// Fraction of failed pages tolerated before a document fails.
    pub max_failed_page_fraction: f32,
    /// Where ingest spools originals (the microVM's scratch disk). Default: system temp dir.
    pub scratch_dir: Option<PathBuf>,
    /// Render child processes per ingest worker. Default: available CPUs.
    pub render_processes: Option<usize>,
    /// A page that takes longer fails its document permanently.
    pub page_render_timeout_s: u64,
    /// Address-space limit per render child.
    pub render_mem_limit_mb: Option<u64>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 32,
            interactive_share: 0.3,
            render_dpi: 200,
            max_failed_page_fraction: 0.05,
            scratch_dir: None,
            render_processes: None,
            page_render_timeout_s: 60,
            render_mem_limit_mb: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TelemetryConfig {
    pub otlp_endpoint: Option<String>,
    pub metrics_addr: Option<String>,
    #[serde(default)]
    pub json_logs: bool,
}

impl Config {
    /// Loads TOML, then applies `VS__SECTION__KEY` environment overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut value: toml::Value = toml::from_str(&text)?;
        apply_env_overrides(&mut value, std::env::vars());
        Ok(value.try_into()?)
    }
}

fn apply_env_overrides(value: &mut toml::Value, vars: impl Iterator<Item = (String, String)>) {
    'vars: for (k, v) in vars {
        let Some(rest) = k.strip_prefix("VS__") else { continue };
        let path: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
        let Some((leaf, parents)) = path.split_last() else { continue };
        let mut cur = &mut *value;
        for seg in parents {
            // Match on the reference by value so the borrow moves into `cur`.
            cur = match cur {
                toml::Value::Table(t) => t.entry(seg.clone()).or_insert_with(|| toml::Value::Table(Default::default())),
                _ => continue 'vars, // a scalar is in the way; ignore the override
            };
        }
        if let toml::Value::Table(t) = cur {
            t.insert(leaf.clone(), parse_scalar(&v));
        }
    }
}

fn parse_scalar(v: &str) -> toml::Value {
    v.parse::<i64>()
        .map(toml::Value::Integer)
        .or_else(|_| v.parse::<f64>().map(toml::Value::Float))
        .or_else(|_| v.parse::<bool>().map(toml::Value::Boolean))
        .unwrap_or_else(|_| toml::Value::String(v.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_sets_nested_key() {
        let mut v: toml::Value = toml::from_str("[worker]\nmax_in_flight = 4\n").unwrap();
        apply_env_overrides(&mut v, vec![("VS__WORKER__MAX_IN_FLIGHT".into(), "64".into())].into_iter());
        assert_eq!(v["worker"]["max_in_flight"].as_integer(), Some(64));
    }

    #[test]
    fn example_configs_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        for name in ["worker.example.toml", "worker.dev.toml"] {
            let cfg = Config::load(&root.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(cfg.blob.bucket, "visual-suite");
            assert!(cfg.classify.is_table());
        }
    }

    #[test]
    fn override_through_scalar_is_ignored() {
        let mut v: toml::Value = toml::from_str("worker = 1\n").unwrap();
        apply_env_overrides(&mut v, vec![("VS__WORKER__X".into(), "2".into())].into_iter());
        assert_eq!(v["worker"].as_integer(), Some(1));
    }
}
