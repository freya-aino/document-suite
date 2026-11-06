//! OpenAI-compatible client (vLLM, SGLang, hosted APIs). See SPEC.md "Client".
//!
//! Per model: a concurrency semaphore (`max_concurrency`), round-robin over endpoints with
//! passive health ejection (3 consecutive failures → endpoint ejected for 30 s), and
//! retries for connection errors / HTTP 429 / HTTP 503 (up to 2 retries, jittered backoff).

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::registry::{ModelRegistry, ModelSpec};

/// Consecutive failures after which an endpoint is ejected.
pub const EJECT_AFTER_FAILURES: u32 = 3;
/// How long an ejected endpoint stays out of rotation.
pub const EJECT_FOR: Duration = Duration::from_secs(30);
/// Retries after the first attempt for retryable failures.
pub const MAX_RETRIES: u32 = 2;
/// Base of the exponential backoff (`base * 2^attempt`, ±50 % jitter).
const BACKOFF_BASE_MS: u64 = 200;

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
    #[error("model configuration: {0}")]
    Config(String),
}

impl ModelError {
    /// Connection errors and HTTP 429/503 are retried; everything else is final.
    pub fn is_retryable(&self) -> bool {
        match self {
            ModelError::Http(_) => true,
            ModelError::Status { status, .. } => matches!(status, 429 | 503),
            _ => false,
        }
    }

    /// Whether this failure counts against the endpoint's health (429 is back-pressure, not
    /// an unhealthy endpoint).
    fn counts_as_endpoint_failure(&self) -> bool {
        match self {
            ModelError::Http(_) => true,
            ModelError::Status { status, .. } => *status >= 500,
            _ => false,
        }
    }
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

#[derive(Debug, Default)]
struct EndpointHealth {
    consecutive_failures: u32,
    ejected_until: Option<Instant>,
}

struct ModelState {
    limit: Arc<Semaphore>,
    rr: AtomicUsize,
    health: Vec<Mutex<EndpointHealth>>,
}

/// OpenAI-compatible client (vLLM, SGLang, hosted APIs).
pub struct OpenAiClient {
    http: reqwest::Client,
    registry: ModelRegistry,
    models: HashMap<String, ModelState>,
    /// xorshift state for backoff jitter (no `rand` dependency needed).
    jitter: AtomicU64,
}

impl OpenAiClient {
    pub fn new(registry: ModelRegistry) -> Self {
        Self::with_http(registry, reqwest::Client::new())
    }

    pub fn with_http(registry: ModelRegistry, http: reqwest::Client) -> Self {
        let models = registry
            .all()
            .map(|m| {
                let state = ModelState {
                    limit: Arc::new(Semaphore::new(m.max_concurrency.max(1))),
                    rr: AtomicUsize::new(0),
                    health: m.endpoints.iter().map(|_| Mutex::new(EndpointHealth::default())).collect(),
                };
                (m.id.clone(), state)
            })
            .collect();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        Self { http, registry, models, jitter: AtomicU64::new(seed) }
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    pub fn spec(&self, model: &str) -> Result<&ModelSpec, ModelError> {
        self.registry.get(model).ok_or_else(|| ModelError::UnknownModel(model.to_string()))
    }

    /// Requests currently in flight towards `model` (0 for unknown models).
    pub fn in_flight(&self, model: &str) -> usize {
        match (self.models.get(model), self.registry.get(model)) {
            (Some(st), Some(spec)) => spec.max_concurrency.max(1).saturating_sub(st.limit.available_permits()),
            _ => 0,
        }
    }

    pub fn build_body(spec: &ModelSpec, req: &ChatRequest) -> Value {
        // TODO(image_transport = "url"): upload very large pages to the blob store and send a
        // presigned URL instead of a data URL. Until then every transport inlines base64.
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
        let body = Self::build_body(spec, req);
        let v = self.post_json(spec, "chat/completions", &body).await?;
        parse_chat_response(&v)
    }

    /// `POST {endpoint}/embeddings` for one batch of texts; vectors in input order.
    pub async fn embeddings(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        let spec = self.spec(model)?;
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let body = json!({
            "model": spec.served_name.clone().unwrap_or_else(|| spec.id.clone()),
            "input": texts,
            "encoding_format": "float",
        });
        let v = self.post_json(spec, "embeddings", &body).await?;
        parse_embeddings_response(&v, texts.len())
    }

    /// Shared request path: semaphore, endpoint selection, retries, health accounting, metrics.
    async fn post_json(&self, spec: &ModelSpec, path: &str, body: &Value) -> Result<Value, ModelError> {
        let state = self.models.get(&spec.id).ok_or_else(|| ModelError::UnknownModel(spec.id.clone()))?;
        let _permit = state.limit.acquire().await.expect("semaphore never closed");
        let in_flight = spec.max_concurrency.max(1).saturating_sub(state.limit.available_permits());
        tracing::trace!(target: "vs_models::metrics", model = %spec.id, in_flight, "model_in_flight");
        let t0 = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            let (idx, endpoint) = self.pick_endpoint(spec, state);
            let span = tracing::debug_span!("model_call", model = %spec.id, endpoint = %endpoint, path, attempt);
            let _g = span.enter();
            let outcome = self.send_once(spec, endpoint, path, body).await;
            let status_label = match &outcome {
                Ok(_) => "ok".to_string(),
                Err(ModelError::Status { status, .. }) => status.to_string(),
                Err(ModelError::Http(_)) => "connection_error".to_string(),
                Err(e) => format!("{e:?}").to_ascii_lowercase(),
            };
            match outcome {
                Ok(v) => {
                    self.record_success(state, idx);
                    let (p, c) = usage(&v);
                    tracing::info!(
                        target: "vs_models::metrics",
                        model = %spec.id, status = %status_label, latency_ms = t0.elapsed().as_millis() as u64,
                        prompt_tokens = p, completion_tokens = c, attempts = attempt + 1, endpoint = %endpoint,
                        "model_request"
                    );
                    return Ok(v);
                }
                Err(e) => {
                    if e.counts_as_endpoint_failure() {
                        self.record_failure(spec, state, idx, endpoint);
                    } else {
                        // 4xx: the endpoint answered, so it is healthy.
                        self.record_success(state, idx);
                    }
                    if !e.is_retryable() || attempt >= MAX_RETRIES {
                        tracing::warn!(
                            target: "vs_models::metrics",
                            model = %spec.id, status = %status_label, latency_ms = t0.elapsed().as_millis() as u64,
                            attempts = attempt + 1, endpoint = %endpoint, error = %e, "model_request"
                        );
                        return Err(e);
                    }
                    tracing::debug!(model = %spec.id, endpoint = %endpoint, attempt, error = %e, "retrying model call");
                    attempt += 1;
                }
            }
            drop(_g);
            tokio::time::sleep(self.backoff(attempt)).await;
        }
    }

    async fn send_once(&self, spec: &ModelSpec, endpoint: &str, path: &str, body: &Value) -> Result<Value, ModelError> {
        let url = format!("{}/{}", endpoint.trim_end_matches('/'), path);
        let mut rb = self.http.post(url).timeout(Duration::from_millis(spec.timeout_ms)).json(body);
        if let Some(var) = &spec.api_key_env {
            match std::env::var(var) {
                Ok(key) if !key.is_empty() => rb = rb.bearer_auth(key),
                _ => tracing::warn!(model = %spec.id, env = %var, "api_key_env is set but the variable is empty or missing"),
            }
        }
        let resp = rb.send().await.map_err(|e| ModelError::Http(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            resp.json::<Value>().await.map_err(|e| ModelError::Response(e.to_string()))
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(ModelError::Status { status: status.as_u16(), body })
        }
    }

    /// Round-robin over healthy endpoints; if every endpoint is ejected, the one whose
    /// ejection ends first is used anyway (better to try than to fail without a request).
    fn pick_endpoint<'s>(&self, spec: &'s ModelSpec, state: &ModelState) -> (usize, &'s str) {
        let n = spec.endpoints.len();
        let start = state.rr.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut fallback: Option<(usize, Instant)> = None;
        for k in 0..n {
            let i = (start + k) % n;
            let mut h = state.health[i].lock().expect("health lock");
            match h.ejected_until {
                Some(until) if until > now => {
                    if fallback.map_or(true, |(_, u)| until < u) {
                        fallback = Some((i, until));
                    }
                }
                Some(_) => {
                    // Ejection expired: give it one probe.
                    h.ejected_until = None;
                    h.consecutive_failures = 0;
                    return (i, &spec.endpoints[i]);
                }
                None => return (i, &spec.endpoints[i]),
            }
        }
        let i = fallback.map(|(i, _)| i).unwrap_or(start % n);
        (i, &spec.endpoints[i])
    }

    fn record_success(&self, state: &ModelState, idx: usize) {
        let mut h = state.health[idx].lock().expect("health lock");
        h.consecutive_failures = 0;
        h.ejected_until = None;
    }

    fn record_failure(&self, spec: &ModelSpec, state: &ModelState, idx: usize, endpoint: &str) {
        let mut h = state.health[idx].lock().expect("health lock");
        h.consecutive_failures += 1;
        if h.consecutive_failures >= EJECT_AFTER_FAILURES && h.ejected_until.is_none() {
            h.ejected_until = Some(Instant::now() + EJECT_FOR);
            tracing::warn!(
                target: "vs_models::metrics",
                model = %spec.id, endpoint = %endpoint, failures = h.consecutive_failures,
                eject_secs = EJECT_FOR.as_secs(), "endpoint_ejected"
            );
        }
    }

    /// Whether `endpoint` of `model` is currently ejected (for tests and diagnostics).
    pub fn is_ejected(&self, model: &str, endpoint: &str) -> bool {
        let Some(spec) = self.registry.get(model) else { return false };
        let Some(state) = self.models.get(model) else { return false };
        let Some(i) = spec.endpoints.iter().position(|e| e == endpoint) else { return false };
        let h = state.health[i].lock().expect("health lock");
        h.ejected_until.is_some_and(|u| u > Instant::now())
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let base = BACKOFF_BASE_MS * 2u64.pow(attempt.min(6));
        // xorshift64*
        let mut x = self.jitter.load(Ordering::Relaxed);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.jitter.store(x, Ordering::Relaxed);
        let r = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as f64 / (1u64 << 31) as f64; // 0..1
        let jittered = base as f64 * (0.5 + r); // 50 %..150 %
        Duration::from_millis(jittered as u64)
    }
}

fn usage(v: &Value) -> (u32, u32) {
    (
        v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        v["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
    )
}

fn parse_chat_response(v: &Value) -> Result<ChatResponse, ModelError> {
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ModelError::Response("missing choices[0].message.content".into()))?
        .to_string();
    let (prompt_tokens, completion_tokens) = usage(v);
    Ok(ChatResponse { text, prompt_tokens, completion_tokens })
}

fn parse_embeddings_response(v: &Value, expected: usize) -> Result<Vec<Vec<f32>>, ModelError> {
    let data = v["data"].as_array().ok_or_else(|| ModelError::Response("missing data[]".into()))?;
    if data.len() != expected {
        return Err(ModelError::Response(format!("expected {expected} embeddings, got {}", data.len())));
    }
    let mut out: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
    for (i, item) in data.iter().enumerate() {
        let idx = item["index"].as_u64().map(|x| x as usize).unwrap_or(i);
        let emb = item["embedding"]
            .as_array()
            .ok_or_else(|| ModelError::Response(format!("data[{i}].embedding is not an array")))?
            .iter()
            .map(|x| x.as_f64().map(|f| f as f32).ok_or_else(|| ModelError::Response("non-numeric embedding".into())))
            .collect::<Result<Vec<f32>, _>>()?;
        out.push((idx, emb));
    }
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, e)| e).collect())
}

/// Text/image embeddings for Stage 2.
#[async_trait]
pub trait Embedder: Send + Sync {
    fn model_ref(&self) -> String;
    fn dimensions(&self) -> usize;
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError>;
}

/// `Embedder` over `POST {endpoint}/embeddings`, batching `batch_size` texts per request.
pub struct OpenAiEmbedder {
    client: Arc<OpenAiClient>,
    model: String,
    model_ref: String,
    batch_size: usize,
    dimensions: usize,
}

impl OpenAiEmbedder {
    /// `batch_size = 0` uses the registry's `batch_size` (default 32). The model must declare
    /// `dimensions` in the registry.
    pub fn new(client: Arc<OpenAiClient>, model: &str, batch_size: usize) -> Result<Self, ModelError> {
        let spec = client.spec(model)?;
        let dimensions = spec
            .dimensions
            .ok_or_else(|| ModelError::Config(format!("model {model:?} has no `dimensions`; required for embeddings")))?;
        let batch_size = if batch_size == 0 { spec.batch_size.unwrap_or(32) } else { batch_size };
        Ok(Self { model_ref: spec.model_ref(), model: model.to_string(), client, batch_size: batch_size.max(1), dimensions })
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn model_ref(&self) -> String {
        self.model_ref.clone()
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch_size) {
            let vecs = self.client.embeddings(&self.model, chunk).await?;
            for v in &vecs {
                if v.len() != self.dimensions {
                    return Err(ModelError::Response(format!(
                        "embedding has {} dimensions, registry says {}",
                        v.len(),
                        self.dimensions
                    )));
                }
            }
            out.extend(vecs);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ModelSpec {
        ModelSpec {
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
            dimensions: None,
            batch_size: None,
            image_transport: None,
        }
    }

    #[test]
    fn body_has_image_and_schema() {
        let req = ChatRequest {
            prompt: "describe".into(),
            images: vec![(Arc::from(vec![1u8, 2, 3]), "image/png")],
            max_tokens: 100,
            json_schema: Some(json!({"type": "object"})),
            ..Default::default()
        };
        let b = OpenAiClient::build_body(&spec(), &req);
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

    #[test]
    fn embeddings_sorted_by_index() {
        let v = json!({"data":[{"index":1,"embedding":[1.0,1.0]},{"index":0,"embedding":[0.0,0.0]}]});
        let e = parse_embeddings_response(&v, 2).unwrap();
        assert_eq!(e, vec![vec![0.0, 0.0], vec![1.0, 1.0]]);
        assert!(parse_embeddings_response(&v, 3).is_err());
    }

    #[test]
    fn retryable_classification() {
        assert!(ModelError::Http("x".into()).is_retryable());
        assert!(ModelError::Status { status: 429, body: String::new() }.is_retryable());
        assert!(ModelError::Status { status: 503, body: String::new() }.is_retryable());
        assert!(!ModelError::Status { status: 400, body: String::new() }.is_retryable());
        assert!(!ModelError::Status { status: 500, body: String::new() }.is_retryable());
    }

    #[test]
    fn backoff_is_jittered_within_bounds() {
        let c = OpenAiClient::new(ModelRegistry::default());
        for attempt in 1..=3 {
            let d = c.backoff(attempt).as_millis() as u64;
            let base = BACKOFF_BASE_MS * 2u64.pow(attempt);
            assert!(d >= base / 2 && d <= base * 3 / 2, "attempt {attempt}: {d} ms");
        }
    }
}
