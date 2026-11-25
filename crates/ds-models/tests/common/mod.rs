//! Minimal HTTP/1.1 mock server for OpenAI-compatible endpoints (no extra dependencies).

#![allow(dead_code)]

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One captured request.
#[derive(Debug, Clone)]
pub struct Captured {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    /// Text of the first user message's text part.
    pub fn user_text(&self) -> String {
        let msgs = self.body["messages"].as_array().cloned().unwrap_or_default();
        for m in msgs {
            if m["role"] == "user" {
                if let Some(parts) = m["content"].as_array() {
                    return parts.iter().find(|p| p["type"] == "text").and_then(|p| p["text"].as_str()).unwrap_or("").to_string();
                }
                return m["content"].as_str().unwrap_or("").to_string();
            }
        }
        String::new()
    }
    pub fn image_count(&self) -> usize {
        self.body["messages"]
            .as_array()
            .map(|ms| ms.iter().filter_map(|m| m["content"].as_array()).flatten().filter(|p| p["type"] == "image_url").count())
            .unwrap_or(0)
    }
}

pub type Handler = dyn Fn(usize, &Captured) -> (u16, Value) + Send + Sync;

pub struct MockServer {
    pub base_url: String,
    pub hits: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<Captured>>>,
}

impl MockServer {
    /// Starts a server; `handler(request_index, request) -> (status, json body)`.
    pub async fn start(handler: Arc<Handler>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(vec![]));
        let (h, r) = (hits.clone(), requests.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let (handler, hits, requests) = (handler.clone(), h.clone(), r.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    // Read headers.
                    let header_end = loop {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = find(&buf, b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let mut lines = head.lines();
                    let req_line = lines.next().unwrap_or_default();
                    let path = req_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let headers: Vec<(String, String)> = lines
                        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                        .collect();
                    let len: usize = headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + len {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body: Value = serde_json::from_slice(&buf[header_end..header_end + len]).unwrap_or(Value::Null);
                    let captured = Captured { path, headers, body };
                    let idx = hits.fetch_add(1, Ordering::SeqCst);
                    requests.lock().unwrap().push(captured.clone());
                    let (status, resp) = handler(idx, &captured);
                    let payload = resp.to_string();
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        503 => "Service Unavailable",
                        _ => "Status",
                    };
                    let out = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        MockServer { base_url: format!("http://{addr}/v1"), hits, requests }
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A chat completion response body with `text` as the assistant message.
pub fn chat_ok(text: &str) -> Value {
    json!({
        "id": "cmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}

/// Registry TOML for one model served by `endpoints`.
pub fn registry_toml(id: &str, family: &str, endpoints: &[&str], extra: &str) -> String {
    let eps = endpoints.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(", ");
    format!(
        "[[model]]\nid = {id:?}\nrevision = \"r1\"\nfamily = {family:?}\nendpoints = [{eps}]\nmax_concurrency = 4\ntimeout_ms = 5000\n{extra}\n"
    )
}
