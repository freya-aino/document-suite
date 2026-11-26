//! A typed client over the suite's public API.
//!
//! The response types are **`ds-api`'s own**, not mirrors of them: `Accepted`, `JobStatus`
//! and `ds_vault::ops::DocumentSummary` are what the handlers serialise, so this client
//! deserialises the same struct the server produced. A mirror would compile for ever
//! while quietly disagreeing with the wire format; this cannot.

use std::time::Duration;

pub use ds_api::{Accepted, JobStatus};
pub use ds_vault::ops::{DocumentSummary, PageSummary};

/// How wide a new upload is, and what to tell the router about it.
#[derive(Debug, Clone)]
pub struct Upload {
    pub language: String,
    pub profile: String,
    /// The caller's document-type hint.
    ///
    /// **Not optional in practice on a born-digital corpus.** With
    /// `classify.skip_vision_for_text_only_digital` on, a text-only digital page skips the
    /// vision classifier, so no document type is determined, confidence falls to
    /// `unknown_doc_type_confidence` (0.3) and every page takes the *low-confidence* DAG —
    /// the router's rule path is then never exercised while every document still
    /// processes successfully. `ds-adapt dataset` writes this into each manifest line and
    /// a real caller sends it.
    pub doc_type: Option<String>,
}

impl Default for Upload {
    fn default() -> Self {
        Self { language: "en".into(), profile: "balanced".into(), doc_type: None }
    }
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    /// Authenticates against the realm with client credentials.
    ///
    /// A real token from the real realm, not a fixture-signed one: the stack runs the
    /// realm and `ds-suite` verifies against its published keys, so a fixture token would
    /// be *correctly* rejected. The token is minted over the published port and accepted
    /// inside the network because the realm's frontend URL is pinned — see
    /// `deploy/compose/README.md`.
    pub async fn login(stack: &crate::Stack, client_id: &str, secret: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let body = [("grant_type", "client_credentials"), ("client_id", client_id), ("client_secret", secret)];
        let res = http.post(&stack.token_url).form(&body).send().await?;
        let status = res.status();
        let text = res.text().await?;
        anyhow::ensure!(status.is_success(), "the realm refused client credentials ({status}): {text}");
        let token = serde_json::from_str::<serde_json::Value>(&text)?
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("no access_token in the realm's answer: {text}"))?;
        Ok(Self { http, base: stack.base_url.clone(), token })
    }

    /// The `iss` claim of the token in hand, for asserting the realm stamps the issuer
    /// the service is configured for — the failure that otherwise reads like a key
    /// problem.
    pub fn token_issuer(&self) -> anyhow::Result<String> {
        let payload = self.token.split('.').nth(1).ok_or_else(|| anyhow::anyhow!("not a JWT"))?;
        let mut b = payload.replace('-', "+").replace('_', "/");
        while !b.len().is_multiple_of(4) {
            b.push('=');
        }
        let bytes = base64_decode(&b).ok_or_else(|| anyhow::anyhow!("payload is not base64"))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        Ok(v["iss"].as_str().unwrap_or_default().to_string())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Status code of a GET with no token at all.
    pub async fn status_without_token(&self, path: &str) -> anyhow::Result<u16> {
        Ok(self.http.get(self.url(path)).send().await?.status().as_u16())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let res = self.http.get(self.url(path)).bearer_auth(&self.token).send().await?;
        let status = res.status();
        let text = res.text().await?;
        anyhow::ensure!(status.is_success(), "GET {path} -> {status}: {text}");
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("GET {path}: {e}; body was {text}"))
    }

    /// Uploads one document as multipart, the way a caller does.
    pub async fn upload(&self, bytes: Vec<u8>, filename: &str, opts: &Upload) -> anyhow::Result<Accepted> {
        let mut options = serde_json::json!({ "language": opts.language, "priority": "batch", "profile": opts.profile });
        if let Some(t) = &opts.doc_type {
            options["doc_type"] = serde_json::json!(t);
        }
        let form = reqwest::multipart::Form::new()
            .part(
                "options",
                reqwest::multipart::Part::text(options.to_string()).mime_str("application/json")?,
            )
            .part(
                "file",
                reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string()).mime_str("application/pdf")?,
            );
        let res = self.http.post(self.url("/v1/documents")).bearer_auth(&self.token).multipart(form).send().await?;
        let status = res.status();
        let text = res.text().await?;
        anyhow::ensure!(status == reqwest::StatusCode::ACCEPTED, "upload -> {status}: {text}");
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("upload: {e}; body was {text}"))
    }

    pub async fn job(&self, job_id: &impl std::fmt::Display) -> anyhow::Result<JobStatus> {
        self.get_json(&format!("/v1/jobs/{job_id}")).await
    }

    /// Polls until the job finishes or the deadline passes.
    pub async fn await_job(&self, job_id: &impl std::fmt::Display, timeout: Duration) -> anyhow::Result<JobStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let s = self.job(job_id).await?;
            if s.finished() {
                return Ok(s);
            }
            anyhow::ensure!(std::time::Instant::now() < deadline, "job {job_id} was still {} after {timeout:?}", s.state);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    pub async fn document(&self, hk: &str) -> anyhow::Result<DocumentSummary> {
        self.get_json(&format!("/v1/documents/{hk}")).await
    }

    /// The stored page result, as served.
    pub async fn page(&self, hk: &str, page_no: u32) -> anyhow::Result<ds_core::PageResult> {
        self.get_json(&format!("/v1/documents/{hk}/pages/{page_no}")).await
    }

    /// Uploads a DAG list as a candidate and activates it.
    pub async fn activate_dag_list(&self, list_json: &str, version: &str) -> anyhow::Result<()> {
        let res = self
            .http
            .post(self.url("/v1/dag-lists"))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(list_json.to_string())
            .send()
            .await?;
        let status = res.status();
        let text = res.text().await?;
        // 409 is a duplicate version, which a re-run is: the list is already there.
        anyhow::ensure!(
            status.is_success() || status == reqwest::StatusCode::CONFLICT,
            "POST /v1/dag-lists -> {status}: {text}"
        );
        let res = self
            .http
            .post(self.url(&format!("/v1/dag-lists/{version}/activate")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = res.status();
        let text = res.text().await?;
        anyhow::ensure!(status.is_success(), "activate {version} -> {status}: {text}");
        Ok(())
    }

    /// The Prometheus exposition from the unauthenticated metrics port.
    pub async fn metrics(&self, stack: &crate::Stack) -> anyhow::Result<crate::promtext::Exposition> {
        let text = self.http.get(&stack.metrics_url).send().await?.text().await?;
        Ok(crate::promtext::Exposition::parse(&text))
    }
}

/// Standard base64, decoded without a dependency. Only ever used on a JWT payload.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = T.iter().position(|&t| t == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jwt_payload_decodes() {
        // base64url with the padding stripped, which is how a JWT carries it.
        let payload = "eyJpc3MiOiJodHRwOi8va2V5Y2xvYWs6ODA4MC9yZWFsbXMva29raSJ9";
        let mut b = payload.replace('-', "+").replace('_', "/");
        while !b.len().is_multiple_of(4) {
            b.push('=');
        }
        let bytes = base64_decode(&b).expect("decodes");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(v["iss"], serde_json::json!("http://keycloak:8080/realms/koki"));
    }

    /// The stage ladder is the only per-document latency source there is, and it now
    /// comes back as `ds-api`'s own struct — so this also pins that the published job
    /// JSON still deserialises into it.
    #[test]
    fn the_stage_ladder_subtracts_across_a_month_boundary() {
        let job: JobStatus = serde_json::from_value(serde_json::json!({
            "job_id": "00000000-0000-0000-0000-000000000001",
            "state": "done",
            "document_hk": null,
            "run_id": "00000000-0000-0000-0000-000000000002",
            "pages_total": 1,
            "pages_done": 1,
            "pages_failed": 0,
            "error": null,
            "created_at": "2026-11-30T23:59:59.900000Z",
            "updated_at": "2026-12-01T00:00:00.100000Z",
            "timings": {
                "created_at": "2026-11-30T23:59:59.900000Z",
                "updated_at": "2026-12-01T00:00:00.100000Z",
                "states": [
                    { "state": "received", "at": "2026-11-30T23:59:59.900000Z" },
                    { "state": "done", "at": "2026-12-01T00:00:00.100000Z" }
                ]
            }
        }))
        .expect("the published job JSON deserialises into ds-api's own JobStatus");
        assert!(job.finished());
        assert_eq!(job.between("received", "done"), Some(200));
        assert_eq!(job.between("received", "never"), None);
    }
}
