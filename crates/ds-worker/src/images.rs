//! `ImageHost` over the blob store, for `image_transport = "url"`.
//!
//! ds-models deliberately knows nothing about blob storage, so the transport needs
//! somewhere to put an image. The worker has both a store and the ability to presign, so
//! it supplies the seam.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use ds_infra::blob::keys;
use ds_infra::BlobStore;
use ds_models::client::ImageHost;
use ds_models::ModelError;

/// How long a presigned image link stays valid. It only has to outlive the model request
/// it was created for, and a model call is bounded by the registry's `timeout_ms`, so an
/// hour is generous. Short on purpose: these are page images, and the link is a bearer
/// credential for whoever holds it.
const LINK_TTL: Duration = Duration::from_secs(3600);

pub struct BlobImageHost {
    pub blob: Arc<dyn BlobStore>,
}

#[async_trait]
impl ImageHost for BlobImageHost {
    async fn host(&self, bytes: &[u8], mime: &str) -> Result<String, ModelError> {
        // Content-addressed: the same page sent to two models uploads once, and a retried
        // task reuses the object instead of writing a second copy.
        let key = keys::model_image(&ds_core::content_sha256(bytes));
        let exists = self.blob.exists(&key).await.map_err(|e| ModelError::Http(e.to_string()))?;
        if !exists {
            self.blob
                .put(&key, bytes.to_vec().into(), mime)
                .await
                .map_err(|e| ModelError::Http(e.to_string()))?;
        }
        self.blob.presign_get(&key, LINK_TTL).await.map_err(|e| ModelError::Http(e.to_string()))
    }
}
