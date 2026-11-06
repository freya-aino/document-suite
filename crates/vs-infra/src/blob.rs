use async_trait::async_trait;
use bytes::Bytes;
use object_store::{
    aws::AmazonS3Builder, path::Path, signer::Signer, Attribute, Attributes, ObjectStore, PutOptions, PutPayload,
};
use reqwest::Method;
use std::sync::Arc;
use std::time::Duration;

use crate::config::BlobConfig;

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
    async fn exists(&self, key: &str) -> Result<bool, BlobError>;
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, BlobError>;
}

/// `object_store`-backed implementation (S3-compatible in production, local FS or
/// memory in tests).
pub struct ObjectBlobStore {
    store: Arc<dyn ObjectStore>,
    /// Present for stores that can presign (S3).
    signer: Option<Arc<dyn Signer>>,
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

    async fn exists(&self, key: &str) -> Result<bool, BlobError> {
        match self.store.head(&Path::from(key)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(BlobError::Store(e.to_string())),
        }
    }

    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, BlobError> {
        let signer = self.signer.as_ref().ok_or_else(|| BlobError::Store("store cannot presign".into()))?;
        signer
            .signed_url(Method::GET, &Path::from(key), ttl)
            .await
            .map(|u| u.to_string())
            .map_err(|e| BlobError::Store(e.to_string()))
    }
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
    pub fn text_layer(document_hk: &str, page_no: u32) -> String {
        format!("textlayer/{document_hk}/{page_no}.json")
    }
    pub fn page_result(document_hk: &str, run_id: &str, page_no: u32) -> String {
        format!("results/{document_hk}/{run_id}/page-{page_no}.json")
    }
    pub fn doc_result(document_hk: &str, run_id: &str) -> String {
        format!("results/{document_hk}/{run_id}/doc.json")
    }
    pub fn crop(page_hk: &str, run_id: &str, block_index: u32) -> String {
        format!("crops/{page_hk}/{run_id}/{block_index}.png")
    }
}
