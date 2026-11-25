use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use ds_classify::{ClassifyError, DocumentClassifier, VisionClassifier, VisionSignals};
use ds_core::{DocTypeSource, DocumentType, LanguageTag};
use ds_dag::PageImage;

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

/// The 11 `DocType` variants, as the schema enum and as the prompt lists them.
const DOC_TYPES: [&str; 11] = [
    "scientific_paper",
    "report",
    "invoice",
    "receipt",
    "form",
    "contract",
    "letter",
    "slide",
    "drawing",
    "handwritten_note",
    "other",
];

/// Document-level classifier: one call over the first few page thumbnails.
///
/// Worth its own model call because a page classifier structurally cannot see what this
/// sees. Asked about a signature page on its own, the right answer *is*
/// `handwritten_note`; asked about it as page 12 of a contract, it is `contract`. The
/// per-page vote can only ever discount that page, never reinterpret it.
pub struct VlmDocumentClassifier {
    pub client: Arc<OpenAiClient>,
    pub model: String,
    /// Opening pages shown. Beyond a handful adds tokens without adding evidence — the
    /// pages that identify a document are at the front.
    pub max_pages: usize,
}

#[async_trait]
impl DocumentClassifier for VlmDocumentClassifier {
    async fn classify(&self, thumbnails: &[PageImage], language: &LanguageTag) -> Result<DocumentType, ClassifyError> {
        if thumbnails.is_empty() {
            return Err(ClassifyError::Vision("no thumbnails to classify".into()));
        }
        let schema = json!({
            "type": "object",
            "properties": {
                "doc_type": { "type": "string", "enum": DOC_TYPES },
                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
            },
            "required": ["doc_type", "confidence"],
            "additionalProperties": false
        });
        let images: Vec<_> = thumbnails
            .iter()
            .take(self.max_pages.max(1))
            .map(|t| (t.bytes.clone(), "image/jpeg"))
            .collect();
        let shown = images.len();
        let resp = self
            .client
            .chat(&self.model, &ChatRequest {
                prompt: prompts::DOCUMENT_CLASSIFY.render(&[("language", language.as_str())]),
                images,
                max_tokens: 100,
                json_schema: Some(schema),
                ..Default::default()
            })
            .await
            .map_err(|e| ClassifyError::Vision(e.to_string()))?;

        #[derive(serde::Deserialize)]
        struct Answer {
            doc_type: ds_core::DocType,
            confidence: f32,
        }
        let a: Answer = serde_json::from_str(resp.text.trim())
            .map_err(|e| ClassifyError::Vision(format!("bad JSON {:?}: {e}", resp.text)))?;
        tracing::debug!(pages_shown = shown, doc_type = ?a.doc_type, confidence = a.confidence, "document classified");
        Ok(DocumentType {
            doc_type: a.doc_type,
            confidence: a.confidence.clamp(0.0, 1.0),
            source: DocTypeSource::Model,
        })
    }
}
