use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub revision: String,
    /// Selects the output parser (see parsers.rs).
    pub family: String,
    #[serde(default)]
    pub task: Vec<String>,
    pub endpoints: Vec<String>,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub languages: Vec<String>,
    /// Env var holding an API key (hosted APIs only).
    pub api_key_env: Option<String>,
    /// Name the server knows the model by (defaults to `id`).
    pub served_name: Option<String>,
}

fn default_concurrency() -> usize {
    64
}
fn default_timeout() -> u64 {
    60_000
}

impl ModelSpec {
    pub fn model_ref(&self) -> String {
        format!("{}@{}", self.id, self.revision)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    models: HashMap<String, ModelSpec>,
}

#[derive(Debug, Deserialize)]
struct File {
    model: Vec<ModelSpec>,
}

impl ModelRegistry {
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        let f: File = toml::from_str(text)?;
        Ok(Self { models: f.model.into_iter().map(|m| (m.id.clone(), m)).collect() })
    }

    pub fn load(path: &Path) -> anyhow_like::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::from_toml(&text).map_err(|e| e.to_string())
    }

    pub fn get(&self, id: &str) -> Option<&ModelSpec> {
        self.models.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models.values()
    }
}

/// Minimal string-error result to avoid pulling anyhow into a library crate.
pub mod anyhow_like {
    pub type Result<T> = std::result::Result<T, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry() {
        let r = ModelRegistry::from_toml(
            r#"
            [[model]]
            id = "paddleocr-vl-1.5"
            revision = "2026-01"
            family = "paddleocr_vl"
            endpoints = ["http://gpu-a:8000/v1"]
            "#,
        )
        .unwrap();
        let m = r.get("paddleocr-vl-1.5").unwrap();
        assert_eq!(m.model_ref(), "paddleocr-vl-1.5@2026-01");
        assert_eq!(m.max_concurrency, 64);
    }
}
