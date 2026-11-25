use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// `Default` exists for test construction and for `..Default::default()` when building a
/// spec in code. It does not weaken config parsing: `id`, `revision`, `family` and
/// `endpoints` have no serde default, so a TOML entry must still supply them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub revision: String,
    /// Selects the output parser (see parsers.rs).
    pub family: String,
    #[serde(default)]
    pub task: Vec<String>,
    pub endpoints: Vec<String>,
    /// Endpoints reserved for interactive work.
    ///
    /// The suite reserves *page slots* for interactive documents on its own side
    /// (`worker.interactive_share`), which stops a batch from filling this copy — and does
    /// nothing at the GPU, where the batch's requests and the interactive one queue
    /// together. A replica set that only interactive work is sent to is the half that was
    /// missing: `deploy/gpu` runs it, and this is how a model declares it.
    ///
    /// Empty (the default) means every request goes to `endpoints`, which is what happened
    /// before and remains right for a deployment with one pool.
    #[serde(default)]
    pub interactive_endpoints: Vec<String>,
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
    /// Embedding models: vector size returned by `/embeddings`. Required for `OpenAiEmbedder`.
    pub dimensions: Option<usize>,
    /// Embedding models: texts per `/embeddings` request (default 32).
    pub batch_size: Option<usize>,
    /// `"base64"` (default) inlines images as data URLs; `"url"` uploads them through the
    /// client's `ImageHost` and sends a link instead (see `client.rs`).
    pub image_transport: Option<String>,

    // ── Capabilities ──
    // Declared rather than discovered, so a new model is a registry entry and not a code
    // change. All optional: left unset, each falls back to what the family implies, which
    // is what every pre-existing registry file relies on.
    /// Whether the server honours `response_format: json_schema` (vLLM guided decoding).
    /// Unset → true for `generic_json` and `auto`, false for the document-VLM families,
    /// which are trained on fixed task strings that guided decoding fights with.
    pub supports_json_schema: Option<bool>,
    /// Whether the model accepts images at all. Unset → true. Setting it to `false` turns
    /// a confusing mid-pipeline model error into a clear rejection at the step.
    pub supports_vision: Option<bool>,
    /// Longest image side the model accepts, in pixels. Unset → send as rendered.
    pub max_image_px: Option<u32>,
    /// Prefix a proxy needs on the model name, e.g. LiteLLM's `hosted_vllm`. Joined to the
    /// served name with `/`.
    pub provider: Option<String>,
    /// Overrides bbox-convention detection: `fractions`, `thousandths` or `pixels`.
    /// Only consulted by the `auto` and `generic_json` parsers; the document-VLM families
    /// know their own convention.
    pub coord_scale: Option<String>,
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

    /// The name to put in the request body: `served_name` or `id`, behind `provider` when
    /// a proxy needs one.
    pub fn served_model(&self) -> String {
        let name = self.served_name.clone().unwrap_or_else(|| self.id.clone());
        match self.provider.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => format!("{}/{name}", p.trim_end_matches('/')),
            None => name,
        }
    }

    /// Whether to send a JSON schema with requests to this model.
    pub fn supports_json_schema(&self) -> bool {
        self.supports_json_schema.unwrap_or(matches!(self.family.as_str(), "generic_json" | "auto"))
    }

    /// Whether this model accepts images. Assumed unless declared otherwise, because
    /// every model the suite was built for is a VLM.
    pub fn supports_vision(&self) -> bool {
        self.supports_vision.unwrap_or(true)
    }

    /// True when images should be uploaded and linked rather than inlined as base64.
    pub fn images_by_url(&self) -> bool {
        self.image_transport.as_deref() == Some("url")
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
        assert_eq!(m.dimensions, None);
    }

    #[test]
    fn capabilities_default_from_family() {
        let r = ModelRegistry::from_toml(
            r#"
            [[model]]
            id = "doc-vlm"
            revision = "1"
            family = "mineru"
            endpoints = ["http://a/v1"]

            [[model]]
            id = "general"
            revision = "1"
            family = "generic_json"
            endpoints = ["http://b/v1"]

            [[model]]
            id = "proxied"
            revision = "1"
            family = "auto"
            endpoints = ["http://c/v1"]
            served_name = "GLM-OCR"
            provider = "hosted_vllm"
            supports_json_schema = false
            supports_vision = false
            "#,
        )
        .unwrap();
        // A document VLM is trained on fixed task strings: no guided decoding by default.
        assert!(!r.get("doc-vlm").unwrap().supports_json_schema());
        assert!(r.get("general").unwrap().supports_json_schema());
        // `auto` would default to true, but an explicit declaration always wins.
        let proxied = r.get("proxied").unwrap();
        assert!(!proxied.supports_json_schema());
        assert!(!proxied.supports_vision());
        assert_eq!(proxied.served_model(), "hosted_vllm/GLM-OCR");
        // Vision is assumed, and no provider means the bare served name.
        assert!(r.get("doc-vlm").unwrap().supports_vision());
        assert_eq!(r.get("doc-vlm").unwrap().served_model(), "doc-vlm");
    }

    #[test]
    fn parses_embedding_fields() {
        let r = ModelRegistry::from_toml(
            r#"
            [[model]]
            id = "embed"
            revision = "1"
            family = "embedding"
            endpoints = ["http://e/v1"]
            dimensions = 1024
            batch_size = 16
            api_key_env = "EMBED_KEY"
            "#,
        )
        .unwrap();
        let m = r.get("embed").unwrap();
        assert_eq!((m.dimensions, m.batch_size), (Some(1024), Some(16)));
        assert_eq!(m.api_key_env.as_deref(), Some("EMBED_KEY"));
    }
}
