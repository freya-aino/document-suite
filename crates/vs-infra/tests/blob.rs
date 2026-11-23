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
        // Garage encrypts at the volume, and sending an SSE header it does not implement
        // would fail every write. That asymmetry is why `blob.sse` defaults to `none`.
        sse: "none".into(),
        sse_kms_key_id: None,
        sse_customer_key_env: None,
        retention: Default::default()
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

    // Deleting something that is not there is not an error: the callers here are making
    // the store match a desired state, not asserting what used to be in it.
    store.delete(&key).await.unwrap();
    assert!(!store.exists(&key).await.unwrap());
    store.delete(&key).await.unwrap();
}

/// Retention, against the real store: `prune` deletes what is older than the cutoff under
/// one prefix, and nothing else.
#[tokio::test]
#[ignore = "needs Garage from `devenv up`"]
async fn prune_deletes_only_what_is_past_the_cutoff() {
    let store = ObjectBlobStore::s3(&cfg()).unwrap();
    let run = uuid::Uuid::now_v7();
    let doomed = format!("it-prune/{run}/old.txt");
    let kept_elsewhere = format!("it-keep/{run}/new.txt");
    for key in [&doomed, &kept_elsewhere] {
        store.put(key, Bytes::from_static(b"x"), "text/plain").await.unwrap();
    }

    // Everything was written just now, so a cutoff in the past must delete nothing — the
    // assertion that the comparison is on the object's own timestamp and not on the
    // prefix alone.
    let none = store.prune(&format!("it-prune/{run}/"), chrono::Utc::now() - chrono::TimeDelta::hours(1)).await.unwrap();
    assert_eq!(none, 0, "pruned an object younger than the cutoff");

    let n = store.prune(&format!("it-prune/{run}/"), chrono::Utc::now() + chrono::TimeDelta::hours(1)).await.unwrap();
    assert_eq!(n, 1);
    assert!(!store.exists(&doomed).await.unwrap());
    assert!(store.exists(&kept_elsewhere).await.unwrap(), "another prefix was pruned");
    store.delete(&kept_elsewhere).await.unwrap();
}
