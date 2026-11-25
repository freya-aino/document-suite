//! OpenAI-compatible client (vLLM, SGLang, hosted APIs). See SPEC.md "Client".
//!
//! Per model: a concurrency semaphore (`max_concurrency`), round-robin over endpoints with
//! passive health ejection (3 consecutive failures → endpoint ejected for 30 s), and
//! retries for connection errors / HTTP 429 / HTTP 503 (up to 2 retries, jittered backoff).

use async_trait::async_trait;
use base64::Engine;
use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::Instrument;

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

/// `model_requests_total{model,status}`, `model_request_duration_seconds{model}` and
/// `model_request_attempts` for one completed call, successful or not.
///
/// The duration covers every attempt, because what a caller waits for is the whole call,
/// not the last try.
fn record_request(spec: &ModelSpec, status: &'static str, elapsed: Duration, attempts: u32) {
    metrics::counter!("model_requests_total", "model" => spec.id.clone(), "status" => status).increment(1);
    metrics::histogram!("model_request_duration_seconds", "model" => spec.id.clone()).record(elapsed.as_secs_f64());
    metrics::histogram!("model_request_attempts", "model" => spec.id.clone()).record(attempts as f64);
}

/// A bounded label for `model_requests_total{status}`.
///
/// The `status_label` used in the log event alongside it is deliberately richer, because a
/// log line can carry anything. A metric label cannot: `ModelError::Response` embeds the
/// offending response text, so one series per distinct malformed reply would grow the
/// series set without limit. HTTP status codes are bounded, so they stay.
fn metric_status(e: &ModelError) -> &'static str {
    match e {
        ModelError::Http(_) => "connection_error",
        ModelError::Status { status, .. } => match status {
            400..=499 => "client_error",
            500..=599 => "server_error",
            _ => "other_status",
        },
        ModelError::Response(_) => "bad_response",
        ModelError::Config(_) => "config",
        ModelError::UnknownModel(_) => "unknown_model",
    }
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
    /// Whether this call is on behalf of an interactive upload.
    ///
    /// It decides which replica set serves it, when the model declares
    /// `interactive_endpoints`. Without that, reserving page slots in the suite stops a
    /// batch from filling *this copy* and does nothing about the batch's requests already
    /// queued at the GPU — the starvation the deployment spec names.
    pub interactive: bool,
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

/// Reports `model_in_flight` while a call holds a permit, and again when it releases it.
///
/// The figure is read from the semaphore rather than counted, so it cannot drift — but a
/// gauge that is only ever `set` on the way in reads the peak of the last burst while the
/// model sits idle, which is the opposite of what a gauge is for.
struct InFlight<'a> {
    model: &'a str,
    limit: &'a Semaphore,
    max: usize,
}

impl InFlight<'_> {
    fn report(&self) {
        let in_flight = self.max.saturating_sub(self.limit.available_permits());
        tracing::trace!(target: "ds_models::metrics", model = %self.model, in_flight, "model_in_flight");
        metrics::gauge!("model_in_flight", "model" => self.model.to_string()).set(in_flight as f64);
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.report();
    }
}

/// OpenAI-compatible client (vLLM, SGLang, hosted APIs).
/// Somewhere a model server can fetch an image from.
///
/// Exists so `image_transport = "url"` works without this crate taking a dependency on
/// the blob store: the worker already has one and implements this over
/// `BlobStore::presign_get`. A very large page sent as base64 inflates by a third and has
/// to be held in memory on both sides; a link does not.
#[async_trait::async_trait]
pub trait ImageHost: Send + Sync {
    /// Stores the bytes and returns a URL the model server can GET.
    async fn host(&self, bytes: &[u8], mime: &str) -> Result<String, ModelError>;
}

pub struct OpenAiClient {
    http: reqwest::Client,
    registry: ModelRegistry,
    models: HashMap<String, ModelState>,
    /// xorshift state for backoff jitter (no `rand` dependency needed).
    jitter: AtomicU64,
    /// Set to enable `image_transport = "url"`; without it that transport inlines base64.
    image_host: Option<Arc<dyn ImageHost>>,
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
                    // One health slot per endpoint across both lanes, the shared set
                    // first: `lane` returns the offset into this array.
                    health: (0..m.endpoints.len() + m.interactive_endpoints.len()).map(|_| Mutex::new(EndpointHealth::default())).collect(),
                };
                (m.id.clone(), state)
            })
            .collect();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        Self { http, registry, models, jitter: AtomicU64::new(seed), image_host: None }
    }

    /// Enables `image_transport = "url"` for models that ask for it.
    pub fn with_image_host(mut self, host: Arc<dyn ImageHost>) -> Self {
        self.image_host = Some(host);
        self
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

    /// Inlines every image as a `data:` URL.
    pub fn data_urls(images: &[(Arc<[u8]>, &'static str)]) -> Vec<String> {
        let b64 = base64::engine::general_purpose::STANDARD;
        images.iter().map(|(bytes, mime)| format!("data:{mime};base64,{}", b64.encode(bytes))).collect()
    }

    pub fn build_body(spec: &ModelSpec, req: &ChatRequest) -> Value {
        Self::build_body_with_images(spec, req, &Self::data_urls(&req.images))
    }

    /// `image_urls` are sent as-is, so they may be `data:` URLs or links an `ImageHost`
    /// produced. Both are the same `image_url` content part to the server.
    pub fn build_body_with_images(spec: &ModelSpec, req: &ChatRequest, image_urls: &[String]) -> Value {
        let mut content = vec![];
        for url in image_urls {
            content.push(json!({ "type": "image_url", "image_url": { "url": url } }));
        }
        content.push(json!({ "type": "text", "text": req.prompt }));
        let mut messages = vec![];
        if let Some(sys) = &req.system {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        messages.push(json!({ "role": "user", "content": content }));
        let mut body = json!({
            "model": spec.served_model(),
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
        let images = fit_images(spec, &req.images);
        let urls = match (spec.images_by_url(), &self.image_host) {
            (true, Some(host)) => {
                let mut urls = Vec::with_capacity(images.len());
                for (bytes, mime) in images.iter() {
                    urls.push(host.host(bytes, mime).await?);
                }
                urls
            }
            // Asked for URLs with nowhere to put them. Inlining still works, so this is a
            // warning rather than a failure — but it is worth saying, because the whole
            // point of the setting was to stop inlining.
            (true, None) => {
                tracing::warn!(model = %spec.id, "image_transport = \"url\" but no ImageHost is configured; inlining base64");
                Self::data_urls(&images)
            }
            (false, _) => Self::data_urls(&images),
        };
        let body = Self::build_body_with_images(spec, req, &urls);
        let v = self.post_json_in(spec, "chat/completions", &body, req.interactive).await?;
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
        // Embeddings are Stage 2 work on a whole document: batch by nature, never
        // competing with an interactive page for the reserved replicas.
        let v = self.post_json_in(spec, "embeddings", &body, false).await?;
        parse_embeddings_response(&v, texts.len())
    }

    /// Shared request path: semaphore, endpoint selection, retries, health accounting, metrics.
    ///
    /// `interactive` picks the lane; everything that is not a chat request on behalf of an
    /// upload (embeddings, in particular) is batch work by nature.
    async fn post_json_in(&self, spec: &ModelSpec, path: &str, body: &Value, interactive: bool) -> Result<Value, ModelError> {
        let state = self.models.get(&spec.id).ok_or_else(|| ModelError::UnknownModel(spec.id.clone()))?;
        // Declared before the permit, so it is dropped after it: the figure is derived
        // from the semaphore, and reading it while this call still holds its permit would
        // count the call that just finished.
        let gauge = InFlight { model: &spec.id, limit: &state.limit, max: spec.max_concurrency.max(1) };
        let _permit = state.limit.acquire().await.expect("semaphore never closed");
        gauge.report();
        let t0 = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            let (idx, endpoint) = self.pick_endpoint(spec, state, interactive);
            let span = tracing::debug_span!("model_call", model = %spec.id, endpoint = %endpoint, path, attempt);
            // `.instrument`, not `span.enter()`: a guard held across an await stays
            // entered on the thread when this future yields, and whatever that thread is
            // polled with next records inside this request's span.
            let outcome = self.send_once(spec, endpoint, path, body).instrument(span).await;
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
                        target: "ds_models::metrics",
                        model = %spec.id, status = %status_label, latency_ms = t0.elapsed().as_millis() as u64,
                        prompt_tokens = p, completion_tokens = c, attempts = attempt + 1, endpoint = %endpoint,
                        "model_request"
                    );
                    record_request(spec, "ok", t0.elapsed(), attempt + 1);
                    metrics::counter!("model_tokens_total", "model" => spec.id.clone(), "kind" => "prompt").increment(u64::from(p));
                    metrics::counter!("model_tokens_total", "model" => spec.id.clone(), "kind" => "completion").increment(u64::from(c));
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
                            target: "ds_models::metrics",
                            model = %spec.id, status = %status_label, latency_ms = t0.elapsed().as_millis() as u64,
                            attempts = attempt + 1, endpoint = %endpoint, error = %e, "model_request"
                        );
                        record_request(spec, metric_status(&e), t0.elapsed(), attempt + 1);
                        return Err(e);
                    }
                    tracing::debug!(model = %spec.id, endpoint = %endpoint, attempt, error = %e, "retrying model call");
                    attempt += 1;
                }
            }
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

    /// The endpoint list this request is served from.
    ///
    /// Interactive work goes to the reserved set when the model declares one, and to the
    /// shared set when it does not; batch work never touches the reserved set, which is
    /// what makes it reserved. Returned with the offset into `health`, because the two
    /// lists share one health array — an endpoint's ejection is a property of the
    /// endpoint, not of the lane that happened to notice.
    fn lane<'s>(&self, spec: &'s ModelSpec, interactive: bool) -> (&'s [String], usize) {
        if interactive && !spec.interactive_endpoints.is_empty() {
            (&spec.interactive_endpoints, spec.endpoints.len())
        } else {
            (&spec.endpoints, 0)
        }
    }

    /// Round-robin over healthy endpoints; if every endpoint is ejected, the one whose
    /// ejection ends first is used anyway (better to try than to fail without a request).
    fn pick_endpoint<'s>(&self, spec: &'s ModelSpec, state: &ModelState, interactive: bool) -> (usize, &'s str) {
        let (endpoints, base) = self.lane(spec, interactive);
        let n = endpoints.len();
        let start = state.rr.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut fallback: Option<(usize, Instant)> = None;
        for k in 0..n {
            let i = base + (start + k) % n;
            let mut h = state.health[i].lock().expect("health lock");
            match h.ejected_until {
                Some(until) if until > now => {
                    if fallback.is_none_or(|(_, u)| until < u) {
                        fallback = Some((i, until));
                    }
                }
                Some(_) => {
                    // Ejection expired: give it one probe.
                    h.ejected_until = None;
                    h.consecutive_failures = 0;
                    return (i, &endpoints[i - base]);
                }
                None => return (i, &endpoints[i - base]),
            }
        }
        let i = fallback.map(|(i, _)| i).unwrap_or(base + start % n);
        (i, &endpoints[i - base])
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
                target: "ds_models::metrics",
                model = %spec.id, endpoint = %endpoint, failures = h.consecutive_failures,
                eject_secs = EJECT_FOR.as_secs(), "endpoint_ejected"
            );
            // The endpoint, not the model, is the useful label: ejecting one of four is a
            // capacity question, ejecting all four is an outage.
            metrics::counter!("model_endpoint_ejections_total", "model" => spec.id.clone(), "endpoint" => endpoint.to_string()).increment(1);
        }
    }

    /// Whether `endpoint` of `model` is currently ejected (for tests and diagnostics).
    pub fn is_ejected(&self, model: &str, endpoint: &str) -> bool {
        let Some(spec) = self.registry.get(model) else { return false };
        let Some(state) = self.models.get(model) else { return false };
        // Both lanes, because an endpoint's health belongs to the endpoint: the shared
        // list first, then the reserved one, which is the order `health` is built in.
        let Some(i) = spec.endpoints.iter().chain(&spec.interactive_endpoints).position(|e| e == endpoint) else { return false };
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

/// An image to send with a request: its bytes and its media type.
type InlineImage = (Arc<[u8]>, &'static str);

/// Downscales images whose long side exceeds the model's declared `max_image_px`.
///
/// Returns borrowed bytes when nothing needs doing, which is the common case: declaring a
/// limit should not cost a decode-and-re-encode on every page that is already within it.
/// A decode or encode failure sends the original rather than failing the step — the model
/// server will either cope or say so, and that is a better outcome than refusing a page
/// because we could not shrink it.
fn fit_images<'a>(spec: &ModelSpec, images: &'a [InlineImage]) -> Cow<'a, [InlineImage]> {
    let Some(max_px) = spec.max_image_px.filter(|m| *m > 0) else { return Cow::Borrowed(images) };
    if images.is_empty() {
        return Cow::Borrowed(images);
    }
    let mut out: Option<Vec<InlineImage>> = None;
    for (i, (bytes, _mime)) in images.iter().enumerate() {
        match shrink_to(bytes, max_px) {
            Some(small) => {
                let v = out.get_or_insert_with(|| images.to_vec());
                // Always re-encoded as PNG: lossless, and the step's own MIME may have
                // been JPEG, which we would otherwise be claiming falsely.
                v[i] = (small, "image/png");
                tracing::debug!(model = %spec.id, max_px, "image downscaled for model limit");
            }
            None => continue,
        }
    }
    match out {
        Some(v) => Cow::Owned(v),
        None => Cow::Borrowed(images),
    }
}

/// `None` when the image already fits, cannot be decoded, or will not re-encode.
fn shrink_to(bytes: &[u8], max_px: u32) -> Option<Arc<[u8]>> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w.max(h) <= max_px {
        return None;
    }
    let scale = max_px as f32 / w.max(h) as f32;
    let (nw, nh) = ((w as f32 * scale).round().max(1.0) as u32, (h as f32 * scale).round().max(1.0) as u32);
    let small = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
    let mut png = Vec::new();
    small.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(Arc::from(png))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ModelSpec {
        ModelSpec {
            id: "m".into(),
            revision: "1".into(),
            family: "generic".into(),
            endpoints: vec!["http://x/v1".into()],
            max_concurrency: 1,
            timeout_ms: 1000,
            ..Default::default()
        }
    }

    fn png(w: u32, h: u32) -> Arc<[u8]> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([7, 7, 7]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        Arc::from(out)
    }

    #[test]
    fn served_model_carries_a_provider_prefix() {
        let mut s = spec();
        assert_eq!(OpenAiClient::build_body(&s, &ChatRequest {
            system: None, prompt: "p".into(), images: vec![], max_tokens: 8, json_schema: None, interactive: false,
        })["model"], "m");
        s.served_name = Some("GLM-OCR".into());
        s.provider = Some("hosted_vllm".into());
        let body = OpenAiClient::build_body(&s, &ChatRequest {
            system: None, prompt: "p".into(), images: vec![], max_tokens: 8, json_schema: None, interactive: false,
        });
        assert_eq!(body["model"], "hosted_vllm/GLM-OCR");
    }

    #[test]
    fn images_within_the_limit_are_not_re_encoded() {
        let mut s = spec();
        s.max_image_px = Some(64);
        let original = png(40, 20);
        let images = vec![(original.clone(), "image/png")];
        let fitted = fit_images(&s, &images);
        assert!(matches!(fitted, Cow::Borrowed(_)), "a fitting image must not be decoded and re-encoded");
        assert_eq!(&*fitted[0].0, &*original);
    }

    #[test]
    fn oversized_images_are_downscaled_preserving_aspect() {
        let mut s = spec();
        s.max_image_px = Some(50);
        let images = vec![(png(200, 100), "image/jpeg")];
        let fitted = fit_images(&s, &images);
        assert!(matches!(fitted, Cow::Owned(_)));
        let decoded = image::load_from_memory(&fitted[0].0).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (50, 25));
        // Re-encoded losslessly, so the declared MIME must follow.
        assert_eq!(fitted[0].1, "image/png");
    }

    #[test]
    fn undecodable_bytes_are_sent_as_they_are() {
        let mut s = spec();
        s.max_image_px = Some(10);
        let images = vec![(Arc::from(vec![0u8, 1, 2, 3]) as Arc<[u8]>, "image/png")];
        assert!(matches!(fit_images(&s, &images), Cow::Borrowed(_)));
    }

    #[test]
    fn no_limit_means_no_work() {
        let images = vec![(png(4000, 4000), "image/png")];
        assert!(matches!(fit_images(&spec(), &images), Cow::Borrowed(_)));
    }

    #[test]
    fn hosted_images_are_sent_as_links() {
        let s = spec();
        let req = ChatRequest {
            system: None,
            prompt: "p".into(),
            images: vec![(png(4, 4), "image/png")],
            max_tokens: 8,
            json_schema: None,
            interactive: false,
        };
        // The base64 path inlines a data URL ...
        let inlined = OpenAiClient::build_body(&s, &req);
        let url = inlined["messages"][0]["content"][0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        // ... and the hosted path sends whatever the host returned, unchanged.
        let hosted = OpenAiClient::build_body_with_images(&s, &req, &["https://blob/x.png".to_string()]);
        assert_eq!(hosted["messages"][0]["content"][0]["image_url"]["url"], "https://blob/x.png");
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
