use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::registry::{ModelRegistry, ModelSpec};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("unknown model {0:?}")]
    UnknownModel(String),
    #[error("http: {0}")]
    Http(String),
    #[error("server returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unexpected response: {0}")]
    Response(String),
}

/// A chat request with optional images and optional JSON-schema constrained output.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub system: Option<String>,
    pub prompt: String,
    /// Encoded images (PNG/JPEG bytes) with their MIME type.
    pub images: Vec<(Arc<[u8]>, &'static str)>,
    pub max_tokens: u32,
    pub json_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// OpenAI-compatible client (vLLM, SGLang, hosted APIs).
pub struct OpenAiClient {
    http: reqwest::Client,
    registry: ModelRegistry,
    limits: HashMap<String, Arc<Semaphore>>,
    rr: AtomicUsize,
}

impl OpenAiClient {
    pub fn new(registry: ModelRegistry) -> Self {
        let limits = registry.all().map(|m| (m.id.clone(), Arc::new(Semaphore::new(m.max_concurrency)))).collect();
        Self { http: reqwest::Client::new(), registry, limits, rr: AtomicUsize::new(0) }
    }

    pub fn spec(&self, model: &str) -> Result<&ModelSpec, ModelError> {
        self.registry.get(model).ok_or_else(|| ModelError::UnknownModel(model.to_string()))
    }

    pub fn build_body(spec: &ModelSpec, req: &ChatRequest) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut content = vec![];
        for (bytes, mime) in &req.images {
            content.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{}", b64.encode(bytes)) }
            }));
        }
        content.push(json!({ "type": "text", "text": req.prompt }));
        let mut messages = vec![];
        if let Some(sys) = &req.system {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        messages.push(json!({ "role": "user", "content": content }));
        let mut body = json!({
            "model": spec.served_name.clone().unwrap_or_else(|| spec.id.clone()),
            "messages": messages,
            "temperature": 0,
            "max_tokens": req.max_tokens,
        });
        if let Some(schema) = &req.json_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": "output", "schema": schema, "strict": true }
            });
        }
        body
    }

    pub async fn chat(&self, model: &str, req: &ChatRequest) -> Result<ChatResponse, ModelError> {
        let spec = self.spec(model)?;
        let _permit = self.limits[&spec.id].acquire().await.expect("semaphore never closed");
        let body = Self::build_body(spec, req);
        let mut attempt = 0;
        loop {
            let endpoint = &spec.endpoints[self.rr.fetch_add(1, Ordering::Relaxed) % spec.endpoints.len()];
            let mut rb = self
                .http
                .post(format!("{endpoint}/chat/completions"))
                .timeout(Duration::from_millis(spec.timeout_ms))
                .json(&body);
            if let Some(var) = &spec.api_key_env {
                if let Ok(key) = std::env::var(var) {
                    rb = rb.bearer_auth(key);
                }
            }
            let retryable = match rb.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let v: Value = resp.json().await.map_err(|e| ModelError::Response(e.to_string()))?;
                    return parse_chat_response(&v);
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    if status == 429 || status == 503 {
                        ModelError::Status { status, body }
                    } else {
                        return Err(ModelError::Status { status, body });
                    }
                }
                Err(e) => ModelError::Http(e.to_string()),
            };
            attempt += 1;
            if attempt > 2 {
                return Err(retryable);
            }
            tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
        }
    }
}

fn parse_chat_response(v: &Value) -> Result<ChatResponse, ModelError> {
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ModelError::Response("missing choices[0].message.content".into()))?
        .to_string();
    Ok(ChatResponse {
        text,
        prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
    })
}

/// Text/image embeddings for Stage 2.
#[async_trait]
pub trait Embedder: Send + Sync {
    fn model_ref(&self) -> String;
    fn dimensions(&self) -> usize;
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_image_and_schema() {
        let spec = ModelSpec {
            id: "m".into(),
            revision: "1".into(),
            family: "generic".into(),
            task: vec![],
            endpoints: vec!["http://x/v1".into()],
            max_concurrency: 1,
            timeout_ms: 1000,
            languages: vec![],
            api_key_env: None,
            served_name: None,
        };
        let req = ChatRequest {
            prompt: "describe".into(),
            images: vec![(Arc::from(vec![1u8, 2, 3]), "image/png")],
            max_tokens: 100,
            json_schema: Some(json!({"type": "object"})),
            ..Default::default()
        };
        let b = OpenAiClient::build_body(&spec, &req);
        assert_eq!(b["messages"][0]["content"][0]["type"], "image_url");
        assert_eq!(b["response_format"]["type"], "json_schema");
        assert_eq!(b["temperature"], 0);
    }

    #[test]
    fn parses_response() {
        let v = json!({"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":3,"completion_tokens":1}});
        let r = parse_chat_response(&v).unwrap();
        assert_eq!((r.text.as_str(), r.prompt_tokens), ("hi", 3));
    }
}
