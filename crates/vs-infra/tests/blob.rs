//! Against Garage from `devenv up`. Run with `cargo test -p vs-infra -- --ignored`.

use std::time::Duration;

use bytes::Bytes;
use vs_infra::blob::{BlobError, BlobStore, ObjectBlobStore};
use vs_infra::config::BlobConfig;

fn cfg() -> BlobConfig {
    BlobConfig {
        bucket: "visual-suite".into(),
        endpoint: Some(
            std::env::var("AWS_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:3900".into()),
        ),
        region: Some(std::env::var("AWS_REGION").unwrap_or_else(|_| "garage".into())),
    }
}

#[tokio::test]
#[ignore = "needs Garage from `devenv up`"]
async fn put_get_exists_presign() {
    let store = ObjectBlobStore::s3(&cfg()).unwrap();
    let key = format!("it/{}.txt", uuid::Uuid::now_v7());
    assert!(!store.exists(&key).await.unwrap());
    assert!(matches!(store.get(&key).await, Err(BlobError::NotFound(_))));

    store
        .put(&key, Bytes::from_static(b"hello"), "text/plain")
        .await
        .unwrap();
    assert!(store.exists(&key).await.unwrap());
    assert_eq!(store.get(&key).await.unwrap(), Bytes::from_static(b"hello"));

    let url = store
        .presign_get(&key, Duration::from_secs(60))
        .await
        .unwrap();
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200, "{url}");
    assert_eq!(resp.headers()["content-type"], "text/plain");
    assert_eq!(resp.bytes().await.unwrap(), Bytes::from_static(b"hello"));
}
