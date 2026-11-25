use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use object_store::{
    aws::{AmazonS3Builder, AmazonS3ConfigKey}, path::Path, signer::Signer, Attribute, Attributes, ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, WriteMultipart,
};
use reqwest::Method;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{BlobConfig, RetentionConfig};

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("blob store: {0}")]
    Store(String),
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, key: &str, bytes: Bytes, content_type: &str) -> Result<(), BlobError>;
    async fn get(&self, key: &str) -> Result<Bytes, BlobError>;
    /// Streams an object without holding it in memory.
    async fn get_stream(&self, key: &str) -> Result<BoxStream<'static, Result<Bytes, BlobError>>, BlobError>;
    async fn exists(&self, key: &str) -> Result<bool, BlobError>;
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, BlobError>;
    /// Stores an object from a stream without holding it in memory (multipart upload
    /// on S3). Returns the number of bytes written. The default collects the stream and
    /// calls `put`; stores that can upload in parts override it.
    async fn put_stream(&self, key: &str, mut stream: BoxStream<'static, Result<Bytes, BlobError>>, content_type: &str) -> Result<u64, BlobError> {
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        let n = buf.len() as u64;
        self.put(key, Bytes::from(buf), content_type).await?;
        Ok(n)
    }
    /// Object size, or `None` if it does not exist.
    async fn size(&self, key: &str) -> Result<Option<u64>, BlobError> {
        // Default for stores without metadata access: existence only.
        Ok(if self.exists(key).await? { Some(0) } else { None })
    }

    /// Removes one object. Deleting something that is not there is not an error: every
    /// caller here is making the store match a desired state, not asserting history.
    async fn delete(&self, key: &str) -> Result<(), BlobError>;

    /// Deletes everything under `prefix` last modified before `cutoff`, and returns how
    /// many objects went.
    ///
    /// Taking a cutoff rather than a duration keeps the clock out of the store: the
    /// sweep decides what "old" means and this applies it, which is also what makes it
    /// testable without sleeping.
    async fn prune(&self, prefix: &str, cutoff: chrono::DateTime<chrono::Utc>) -> Result<u64, BlobError>;
}

/// Multipart part size. S3 requires >= 5 MiB for all but the last part.
const MULTIPART_CHUNK: usize = 8 * 1024 * 1024;
/// Parts in flight per upload.
const MULTIPART_CONCURRENCY: usize = 4;

/// `object_store`-backed implementation (S3-compatible in production, local FS or
/// memory in tests).
///
/// `Debug` says which backend and whether it can presign, and nothing about the
/// credentials or the encryption key behind it.
pub struct ObjectBlobStore {
    store: Arc<dyn ObjectStore>,
    /// Present for stores that can presign (S3).
    signer: Option<Arc<dyn Signer>>,
}

impl std::fmt::Debug for ObjectBlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBlobStore").field("store", &self.store.to_string()).field("presigns", &self.signer.is_some()).finish()
    }
}

impl ObjectBlobStore {
    /// S3-compatible store. Credentials come from the environment (`AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, ...); bucket, endpoint and region from config.
    /// Path-style requests, which Garage and most self-hosted servers need.
    pub fn s3(cfg: &BlobConfig) -> Result<Self, BlobError> {
        let mut b = AmazonS3Builder::from_env().with_bucket_name(&cfg.bucket).with_virtual_hosted_style_request(false);
        if let Some(endpoint) = &cfg.endpoint {
            b = b.with_endpoint(endpoint).with_allow_http(endpoint.starts_with("http://"));
        }
        if let Some(region) = &cfg.region {
            b = b.with_region(region);
        }
        b = encryption(b, cfg)?;
        let s3 = Arc::new(b.build().map_err(|e| BlobError::Store(e.to_string()))?);
        Ok(Self { store: s3.clone(), signer: Some(s3) })
    }

    /// Any store without presigning (tests: `object_store::memory::InMemory`).
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store, signer: None }
    }
}

#[async_trait]
impl BlobStore for ObjectBlobStore {
    async fn put(&self, key: &str, bytes: Bytes, content_type: &str) -> Result<(), BlobError> {
        let attributes = if self.signer.is_some() {
            Attributes::from_iter([(Attribute::ContentType, content_type.to_string())])
        } else {
            Attributes::new() // the local-filesystem store rejects attributes
        };
        self.store
            .put_opts(&Path::from(key), PutPayload::from_bytes(bytes), PutOptions { attributes, ..Default::default() })
            .await
            .map(|_| ())
            .map_err(|e| BlobError::Store(e.to_string()))
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        match self.store.get(&Path::from(key)).await {
            Ok(r) => r.bytes().await.map_err(|e| BlobError::Store(e.to_string())),
            Err(object_store::Error::NotFound { .. }) => Err(BlobError::NotFound(key.to_string())),
            Err(e) => Err(BlobError::Store(e.to_string())),
        }
    }

    async fn get_stream(&self, key: &str) -> Result<BoxStream<'static, Result<Bytes, BlobError>>, BlobError> {
        match self.store.get(&Path::from(key)).await {
            Ok(r) => Ok(r.into_stream().map(|c| c.map_err(|e| BlobError::Store(e.to_string()))).boxed()),
            Err(object_store::Error::NotFound { .. }) => Err(BlobError::NotFound(key.to_string())),
            Err(e) => Err(BlobError::Store(e.to_string())),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, BlobError> {
        match self.store.head(&Path::from(key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(BlobError::Store(e.to_string())),
        }
    }

    async fn put_stream(&self, key: &str, mut stream: BoxStream<'static, Result<Bytes, BlobError>>, content_type: &str) -> Result<u64, BlobError> {
        let attributes =
            if self.signer.is_some() { Attributes::from_iter([(Attribute::ContentType, content_type.to_string())]) } else { Attributes::new() };
        let store_err = |e: object_store::Error| BlobError::Store(e.to_string());
        let upload =
            self.store.put_multipart_opts(&Path::from(key), PutMultipartOptions { attributes, ..Default::default() }).await.map_err(store_err)?;
        let mut w = WriteMultipart::new_with_chunk_size(upload, MULTIPART_CHUNK);
        let mut n: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    w.abort().await.ok();
                    return Err(e);
                }
            };
            n += chunk.len() as u64;
            if let Err(e) = w.wait_for_capacity(MULTIPART_CONCURRENCY).await {
                w.abort().await.ok();
                return Err(store_err(e));
            }
            w.write(&chunk);
        }
        w.finish().await.map_err(store_err)?;
        Ok(n)
    }

    async fn size(&self, key: &str) -> Result<Option<u64>, BlobError> {
        match self.store.head(&Path::from(key)).await {
            Ok(m) => Ok(Some(m.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(BlobError::Store(e.to_string())),
        }
    }

    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, BlobError> {
        let signer = self.signer.as_ref().ok_or_else(|| BlobError::Store("store cannot presign".into()))?;
        signer.signed_url(Method::GET, &Path::from(key), ttl).await.map(|u| u.to_string()).map_err(|e| BlobError::Store(e.to_string()))
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        match self.store.delete(&Path::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(BlobError::Store(e.to_string())),
        }
    }

    async fn prune(&self, prefix: &str, cutoff: chrono::DateTime<chrono::Utc>) -> Result<u64, BlobError> {
        let path = Path::from(prefix);
        // Listed and deleted in one pipeline rather than collected first: a prefix with a
        // million objects under it must not become a million `ObjectMeta` in memory.
        let doomed = self
            .store
            .list(Some(&path))
            .filter_map(move |meta| async move {
                match meta {
                    Ok(m) if m.last_modified < cutoff => Some(Ok(m.location)),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .boxed();
        let mut deleted: u64 = 0;
        let mut results = self.store.delete_stream(doomed);
        while let Some(r) = results.next().await {
            match r {
                Ok(_) => deleted += 1,
                // Something else deleted it, or it expired under a bucket rule. The end
                // state is the one asked for.
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(BlobError::Store(e.to_string())),
            }
        }
        Ok(deleted)
    }
}

/// Applies `[blob].sse`.
///
/// A key id that is missing where the mode needs one is a startup error rather than a
/// silent fall back to unencrypted: a deployment that asked for KMS and got nothing would
/// believe it was encrypted.
fn encryption(b: AmazonS3Builder, cfg: &BlobConfig) -> Result<AmazonS3Builder, BlobError> {
    let kms = |what: &str| -> Result<String, BlobError> {
        cfg.sse_kms_key_id
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| BlobError::Store(format!("blob.sse = {what:?} needs blob.sse_kms_key_id")))
    };
    Ok(match cfg.sse.trim().to_ascii_lowercase().as_str() {
        "" | "none" => b,
        // No builder method for plain SSE-S3 in object_store 0.14 — only KMS, DSSE and
        // SSE-C have one — so it goes through the config key the builder parses, which is
        // the same path its own `from_env` uses.
        "aes256" | "sse-s3" => {
            // `S3EncryptionConfigKey` is not re-exported by `object_store`, so the key is
            // named the way the crate's own `from_env` names it and parsed back.
            let key = AmazonS3ConfigKey::from_str("aws_server_side_encryption")
                .map_err(|e| BlobError::Store(format!("object_store does not know the SSE config key: {e}")))?;
            b.with_config(key, "AES256")
        }
        "aws:kms" | "sse-kms" => b.with_sse_kms_encryption(kms("aws:kms")?),
        "aws:kms:dsse" | "dsse-kms" => b.with_dsse_kms_encryption(kms("aws:kms:dsse")?),
        "sse-c" => {
            let var = cfg
                .sse_customer_key_env
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| BlobError::Store("blob.sse = \"sse-c\" needs blob.sse_customer_key_env".into()))?;
            let key = std::env::var(var).map_err(|_| BlobError::Store(format!("blob.sse = \"sse-c\": {var} is not set")))?;
            b.with_ssec_encryption(key)
        }
        other => {
            return Err(BlobError::Store(format!(
                "blob.sse {other:?}: expected none, aes256, aws:kms, aws:kms:dsse or sse-c"
            )))
        }
    })
}

/// One pass of the retention sweep: every class of object with a retention, pruned.
///
/// Returns what it deleted, per prefix, so the caller can log it. Open question 4 of the
/// system spec — retention rules for originals and page images — is answered by the
/// defaults in [`RetentionConfig`] and by this being the thing that applies them.
pub async fn sweep_retention(blob: &dyn BlobStore, cfg: &RetentionConfig) -> Vec<(&'static str, u64)> {
    let now = chrono::Utc::now();
    let mut out = Vec::new();
    for (prefix, days) in [
        ("pages/", cfg.page_images_days),
        ("results/", cfg.results_days),
        ("textlayer/", cfg.text_layers_days),
        ("crops/", cfg.crops_days),
        ("model-images/", cfg.model_images_days),
        ("originals/", cfg.originals_days),
    ] {
        if days == 0 {
            continue;
        }
        let cutoff = now - chrono::TimeDelta::days(days as i64);
        match blob.prune(prefix, cutoff).await {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(prefix, objects = n, days, "pruned objects past their retention");
                out.push((prefix, n));
            }
            Err(e) => tracing::warn!(prefix, error = %e, "retention sweep failed for a prefix"),
        }
    }
    out
}

/// Object key from a `source_uri`: `s3://<bucket>/<key>` or a bare key.
pub fn key_from_uri<'a>(uri: &'a str, bucket: &str) -> &'a str {
    uri.strip_prefix("s3://").and_then(|rest| rest.strip_prefix(bucket)).and_then(|k| k.strip_prefix('/')).unwrap_or(uri)
}

/// Key helpers matching the layout in SPEC.md.
pub mod keys {
    pub fn original(tenant: &str, sha256: &str) -> String {
        format!("originals/{tenant}/{sha256}")
    }
    pub fn page_image(document_hk: &str, page_no: u32, dpi: u32) -> String {
        format!("pages/{document_hk}/{page_no}/{dpi}.png")
    }
    pub fn thumbnail(document_hk: &str, page_no: u32) -> String {
        format!("pages/{document_hk}/{page_no}/thumb.jpg")
    }
    /// Classifier tier-0 signals; written last, so its presence means the page's
    /// assets are complete.
    pub fn signals(document_hk: &str, page_no: u32) -> String {
        format!("pages/{document_hk}/{page_no}/signals.json")
    }
    pub fn text_layer(document_hk: &str, page_no: u32) -> String {
        format!("textlayer/{document_hk}/{page_no}.json")
    }
    pub fn page_result(document_hk: &str, run_id: &str, page_no: u32) -> String {
        format!("results/{document_hk}/{run_id}/page-{page_no}.json")
    }
    /// Scratch object for an image handed to a model by link rather than inlined
    /// (`image_transport = "url"`). Content-addressed, so the same image is uploaded once
    /// however many models ask for it, and a retry reuses the object.
    pub fn model_image(sha256: &str) -> String {
        format!("model-images/{sha256}")
    }

    pub fn doc_result(document_hk: &str, run_id: &str) -> String {
        format!("results/{document_hk}/{run_id}/doc.json")
    }
    pub fn crop(page_hk: &str, run_id: &str, block_index: u32) -> String {
        format!("crops/{page_hk}/{run_id}/{block_index}.png")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(sse: &str) -> BlobConfig {
        BlobConfig {
            bucket: "b".into(),
            endpoint: Some("http://127.0.0.1:1".into()),
            region: Some("r".into()),
            sse: sse.into(),
            sse_kms_key_id: None,
            sse_customer_key_env: None,
            retention: Default::default(),
        }
    }

    #[test]
    fn a_mode_that_needs_a_key_and_does_not_have_one_fails_to_build() {
        // The failure that matters: a deployment that asked for KMS and silently got
        // plaintext would believe its originals were encrypted.
        let err = ObjectBlobStore::s3(&cfg("aws:kms")).expect_err("no key id");
        assert!(format!("{err}").contains("sse_kms_key_id"), "{err}");
        let mut with_key = cfg("aws:kms");
        with_key.sse_kms_key_id = Some("arn:aws:kms:eu-central-1:1:key/abc".into());
        assert!(ObjectBlobStore::s3(&with_key).is_ok());

        let err = ObjectBlobStore::s3(&cfg("sse-c")).expect_err("no key env");
        assert!(format!("{err}").contains("sse_customer_key_env"), "{err}");
    }

    #[test]
    fn the_modes_are_named_and_anything_else_is_refused() {
        for ok in ["none", "", "aes256", "sse-s3", "AES256", " none "] {
            assert!(ObjectBlobStore::s3(&cfg(ok)).is_ok(), "{ok:?} was refused");
        }
        let err = ObjectBlobStore::s3(&cfg("yes please")).expect_err("not a mode");
        assert!(format!("{err}").contains("expected none, aes256"), "{err}");
    }

    #[tokio::test]
    async fn retention_prunes_each_prefix_by_its_own_rule() {
        let store = ObjectBlobStore::new(Arc::new(object_store::memory::InMemory::new()));
        for key in ["pages/doc/1/200.png", "results/doc/run/page-1.json", "originals/t1/abc", "crops/p/r/0.png"] {
            store.put(key, Bytes::from_static(b"x"), "application/octet-stream").await.expect("put");
        }
        // Everything is new, so nothing is past its retention yet — the sweep must be a
        // no-op rather than a cleaner of fresh work.
        let swept = sweep_retention(&store, &RetentionConfig::default()).await;
        assert!(swept.is_empty(), "the sweep deleted objects written a moment ago: {swept:?}");
        for key in ["pages/doc/1/200.png", "originals/t1/abc"] {
            assert!(store.exists(key).await.expect("exists"), "{key} went");
        }

        // A retention of one day against objects written now deletes nothing either; a
        // cutoff is what does, and `prune` is where the clock is applied.
        let n = store.prune("pages/", chrono::Utc::now() + chrono::TimeDelta::days(1)).await.expect("prune");
        assert_eq!(n, 1);
        assert!(!store.exists("pages/doc/1/200.png").await.expect("exists"));
        assert!(store.exists("originals/t1/abc").await.expect("exists"), "a different prefix was pruned");

        // Originals default to forever, which is the one default that must not be a
        // number: deleting a tenant's input is their decision, not this software's.
        assert_eq!(RetentionConfig::default().originals_days, 0);
        let only_originals = RetentionConfig { originals_days: 0, ..Default::default() };
        assert!(sweep_retention(&store, &only_originals).await.is_empty());
    }
}
