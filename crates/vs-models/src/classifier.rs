use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use vs_classify::{ClassifyError, VisionClassifier, VisionSignals};
use vs_core::LanguageTag;
use vs_dag::PageImage;

use crate::client::{ChatRequest, OpenAiClient};
use crate::prompts;

/// Tier 1 page classifier on a small VLM with guided JSON output.
pub struct VlmPageClassifier {
    pub client: Arc<OpenAiClient>,
    pub model: String,
}

#[async_trait]
impl VisionClassifier for VlmPageClassifier {
    async fn classify(&self, thumbnail: &PageImage, language: &LanguageTag) -> Result<VisionSignals, ClassifyError> {
        let p = |name: &str| (name.to_string(), json!({"type": "number", "minimum": 0, "maximum": 1}));
        let mut props = serde_json::Map::new();
        props.insert("doc_type".into(), json!({"type": "string", "enum": [
            "scientific_paper","report","invoice","receipt","form","contract","letter","slide","drawing","handwritten_note","other"]}));
        for name in ["doc_type_confidence", "p_handwriting", "p_tables", "p_formulas", "p_charts", "p_images"] {
            let (k, v) = p(name);
            props.insert(k, v);
        }
        let required: Vec<String> = props.keys().cloned().collect();
        let schema = json!({ "type": "object", "properties": props, "required": required });

        let resp = self
            .client
            .chat(&self.model, &ChatRequest {
                prompt: prompts::PAGE_CLASSIFY.render(&[("language", language.as_str())]),
                images: vec![(thumbnail.bytes.clone(), "image/jpeg")],
                max_tokens: 200,
                json_schema: Some(schema),
                ..Default::default()
            })
            .await
            .map_err(|e| ClassifyError::Vision(e.to_string()))?;
        serde_json::from_str(&resp.text).map_err(|e| ClassifyError::Vision(format!("bad JSON: {e}")))
    }
}
