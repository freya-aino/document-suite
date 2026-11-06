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
    /// `vs-api` only.
    #[serde(default)]
    pub api: ApiConfig,
    /// Passed through to `vs-classify` (`ClassifyConfig`).
    #[serde(default = "empty_table")]
    pub classify: toml::Value,
    /// Stage 2 (`doc` role) and page-classifier models; `page` and `doc` workers only.
    #[serde(default)]
    pub stage2: Stage2Config,
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

/// Models and thresholds of the `page` classifier and the `doc` role. Model names are
/// registry ids (`[models].registry`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Stage2Config {
    /// Extraction LLM.
    pub extraction_model: String,
    /// Verification LLM (may equal `extraction_model`).
    pub verify_model: String,
    /// Text embedder for ontology matching, corpus resolution and graph vectors.
    pub embedding_model: String,
    /// LLM judge for ambiguous corpus-resolution candidates; none = thresholds only.
    pub judge_model: Option<String>,
    /// Tier 1 page classifier (VLM). None = rules-only classification.
    pub classifier_model: Option<String>,
    /// Characters per extraction window.
    pub window_chars: usize,
    /// Cosine threshold for mapping raw types onto the ontology.
    pub type_match_threshold: f32,
    /// Corpus resolution: score at or above which an entity links without a judge.
    pub link_threshold: f32,
    /// Corpus resolution: score band in which the judge decides.
    pub judge_band: (f32, f32),
    /// Texts per embedding request (0 = registry default).
    pub embed_batch_size: usize,
    /// How long a worker trusts its cached "active DAG list" mapping.
    pub dag_list_refresh_s: u64,
}

impl Default for Stage2Config {
    fn default() -> Self {
        Self {
            extraction_model: "extract-llm".into(),
            verify_model: "extract-llm".into(),
            embedding_model: "embed-text".into(),
            judge_model: None,
            classifier_model: None,
            window_chars: 12_000,
            type_match_threshold: 0.85,
            link_threshold: 0.92,
            judge_band: (0.80, 0.92),
            embed_batch_size: 0,
            dag_list_refresh_s: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Multipart upload size limit (413 above it). Default 2 GiB.
    pub max_upload_bytes: u64,
    /// Interactive requests get 429 while the interactive ingest lane has more pending
    /// messages than this.
    pub interactive_max_pending: u64,
    /// Where multipart uploads are spooled while hashing. Default: system temp dir.
    pub scratch_dir: Option<PathBuf>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { max_upload_bytes: 2 * 1024 * 1024 * 1024, interactive_max_pending: 1000, scratch_dir: None }
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
        let Some(rest) = k.strip_prefix("VS__") else {
            continue;
        };
        let path: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
        let Some((leaf, parents)) = path.split_last() else {
            continue;
        };
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
            assert_eq!(cfg.api.interactive_max_pending, 1000);
            assert_eq!(cfg.stage2.embedding_model, "embed-text");
            assert_eq!(cfg.stage2.dag_list_refresh_s, 60);
        }
    }

    #[test]
    fn stage2_defaults_when_section_missing() {
        let v: toml::Value = toml::from_str("x = 1\n").unwrap();
        let s2: Stage2Config = v.get("stage2").cloned().map(|v| v.try_into().unwrap()).unwrap_or_default();
        assert_eq!(s2.extraction_model, "extract-llm");
        assert!(s2.classifier_model.is_none());
        let s2: Stage2Config = toml::from_str("classifier_model = \"page-classifier\"\njudge_band = [0.7, 0.9]\n").unwrap();
        assert_eq!(s2.classifier_model.as_deref(), Some("page-classifier"));
        assert_eq!(s2.judge_band, (0.7, 0.9));
    }

    #[test]
    fn override_through_scalar_is_ignored() {
        let mut v: toml::Value = toml::from_str("worker = 1\n").unwrap();
        apply_env_overrides(&mut v, vec![("VS__WORKER__X".into(), "2".into())].into_iter());
        assert_eq!(v["worker"].as_integer(), Some(1));
    }
}
