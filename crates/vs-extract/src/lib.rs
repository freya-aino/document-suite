//! Stage 2 information extraction. See `SPEC.md`.

pub mod ground;
pub mod model;
pub mod resolve;
pub mod window;

pub use ground::locate_span;
pub use model::{Arg, ArgValue, Assertion, AssertionKind, DocEntity, ExtractionOutput, Mention, Qualifiers};
pub use window::{build_windows, Window};

use async_trait::async_trait;
use vs_core::Profile;
use vs_stitch::StitchedDocument;

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("model: {0}")]
    Model(String),
    #[error("schema violation: {0}")]
    Schema(String),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

/// Full Stage 2 extraction for one document (steps 1–6 in SPEC.md; corpus resolution is separate).
#[async_trait]
pub trait Extractor: Send + Sync {
    async fn extract(&self, doc: &StitchedDocument, profile: Profile) -> Result<ExtractionOutput, ExtractError>;
}

/// LLM-based extractor using `vs-models`.
pub struct LlmExtractor {
    pub client: std::sync::Arc<vs_models::OpenAiClient>,
    pub extraction_model: String,
    pub verify_model: String,
    pub window_chars: usize,
    pub drop_ungrounded: bool,
}

pub const EXTRACTION_SCHEMA: &str = include_str!("../schemas/extraction.schema.json");

#[async_trait]
impl Extractor for LlmExtractor {
    async fn extract(&self, doc: &StitchedDocument, _profile: Profile) -> Result<ExtractionOutput, ExtractError> {
        let windows = build_windows(doc, self.window_chars);
        tracing::debug!(windows = windows.len(), "built extraction windows");
        Err(ExtractError::NotImplemented(
            "per window: prompt + EXTRACTION_SCHEMA → ground quotes → verify → resolve in-doc → normalise types",
        ))
    }
}
