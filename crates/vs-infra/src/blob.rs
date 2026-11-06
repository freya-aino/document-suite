use async_trait::async_trait;
use bytes::Bytes;
use object_store::{path::Path, ObjectStore, PutPayload};
use std::sync::Arc;
use std::time::Duration;

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

/// `object_store`-backed implementation (S3-compatible in production, local FS in tests).
pub struct ObjectBlobStore {
    pub store: Arc<dyn ObjectStore>,
}

#[async_trait]
impl BlobStore for ObjectBlobStore {
    async fn put(&self, key: &str, bytes: Bytes, _content_type: &str) -> Result<(), BlobError> {
        self.store
            .put(&Path::from(key), PutPayload::from_bytes(bytes))
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

    async fn presign_get(&self, _key: &str, _ttl: Duration) -> Result<String, BlobError> {
        todo!("use object_store::signer::Signer on the AmazonS3 store")
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
