mod common;

use common::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ds_models::client::{Embedder, OpenAiEmbedder};
use ds_models::{ChatRequest, ModelError, ModelRegistry, OpenAiClient};

fn client(toml: &str) -> Arc<OpenAiClient> {
    Arc::new(OpenAiClient::new(ModelRegistry::from_toml(toml).unwrap()))
}

fn req(prompt: &str) -> ChatRequest {
    ChatRequest { prompt: prompt.into(), max_tokens: 16, ..Default::default() }
}

#[tokio::test]
async fn chat_round_trip_uses_served_name_and_bearer_key() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert_eq!(c.path, "/v1/chat/completions");
        (200, chat_ok(&format!("echo:{}", c.user_text())))
    }))
    .await;
    std::env::set_var("DS_TEST_API_KEY", "secret-123");
    let c = client(&registry_toml("m", "generic_json", &[&srv.base_url], "served_name = \"vendor/m\"\napi_key_env = \"DS_TEST_API_KEY\""));
    let r = c.chat("m", &req("hello")).await.unwrap();
    assert_eq!((r.text.as_str(), r.prompt_tokens, r.completion_tokens), ("echo:hello", 10, 5));
    let captured = &srv.requests()[0];
    assert_eq!(captured.body["model"], "vendor/m");
    assert_eq!(captured.body["temperature"], 0);
    assert_eq!(captured.header("authorization"), Some("Bearer secret-123"));
    assert!(matches!(c.chat("unknown", &req("x")).await, Err(ModelError::UnknownModel(_))));
}

#[tokio::test]
async fn retries_429_and_503_then_succeeds() {
    let srv = MockServer::start(Arc::new(|i, _| match i {
        0 => (503, json!({"error": "busy"})),
        1 => (429, json!({"error": "rate"})),
        _ => (200, chat_ok("ok")),
    }))
    .await;
    let c = client(&registry_toml("m", "generic_json", &[&srv.base_url], ""));
    let r = c.chat("m", &req("x")).await.unwrap();
    assert_eq!(r.text, "ok");
    assert_eq!(srv.hits(), 3);
}

#[tokio::test]
async fn gives_up_after_two_retries() {
    let srv = MockServer::start(Arc::new(|_, _| (503, json!({"error": "busy"})))).await;
    let c = client(&registry_toml("m", "generic_json", &[&srv.base_url], ""));
    let err = c.chat("m", &req("x")).await.unwrap_err();
    assert!(matches!(err, ModelError::Status { status: 503, .. }), "{err}");
    assert_eq!(srv.hits(), 3, "1 attempt + 2 retries");
}

#[tokio::test]
async fn does_not_retry_4xx() {
    let srv = MockServer::start(Arc::new(|_, _| (400, json!({"error": "bad"})))).await;
    let c = client(&registry_toml("m", "generic_json", &[&srv.base_url], ""));
    let err = c.chat("m", &req("x")).await.unwrap_err();
    assert!(matches!(err, ModelError::Status { status: 400, .. }));
    assert_eq!(srv.hits(), 1);
}

#[tokio::test]
async fn round_robins_and_ejects_dead_endpoint() {
    let srv = MockServer::start(Arc::new(|_, _| (200, chat_ok("ok")))).await;
    // A closed port: every connection attempt fails immediately.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        format!("http://{addr}/v1")
    };
    let c = client(&registry_toml("m", "generic_json", &[&srv.base_url, &dead], ""));
    // Each call that lands on the dead endpoint retries on the live one, so all calls succeed.
    for _ in 0..6 {
        c.chat("m", &req("x")).await.unwrap();
    }
    assert!(c.is_ejected("m", &dead), "dead endpoint ejected after 3 consecutive failures");
    assert!(!c.is_ejected("m", &srv.base_url));
    let before = srv.hits();
    for _ in 0..4 {
        c.chat("m", &req("x")).await.unwrap();
    }
    assert_eq!(srv.hits() - before, 4, "only the healthy endpoint is used while the other is ejected");
}

#[tokio::test]
async fn concurrency_is_bounded_by_max_concurrency() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (f, p) = (in_flight.clone(), peak.clone());
    let srv = MockServer::start(Arc::new(move |_, _| {
        let now = f.fetch_add(1, Ordering::SeqCst) + 1;
        p.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(30));
        f.fetch_sub(1, Ordering::SeqCst);
        (200, chat_ok("ok"))
    }))
    .await;
    let toml = registry_toml("m", "generic_json", &[&srv.base_url], "").replace("max_concurrency = 4", "max_concurrency = 2");
    let c = client(&toml);
    let r = req("x");
    let futs: Vec<_> = (0..8).map(|_| c.chat("m", &r)).collect();
    for r in futures::future::join_all(futs).await {
        r.unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) <= 2, "peak in flight {}", peak.load(Ordering::SeqCst));
    assert_eq!(c.in_flight("m"), 0);
}

#[tokio::test]
async fn embedder_batches_and_checks_dimensions() {
    let srv = MockServer::start(Arc::new(|_, c| {
        assert_eq!(c.path, "/v1/embeddings");
        let inputs = c.body["input"].as_array().unwrap();
        let data: Vec<_> = inputs
            .iter()
            .enumerate()
            .rev() // out of order on purpose
            .map(|(i, t)| json!({"index": i, "embedding": [t.as_str().unwrap().len() as f32, 1.0, 2.0]}))
            .collect();
        (200, json!({"object": "list", "data": data, "usage": {"prompt_tokens": 3}}))
    }))
    .await;
    let c = client(&registry_toml("e", "embedding", &[&srv.base_url], "dimensions = 3\nbatch_size = 2"));
    let emb = OpenAiEmbedder::new(c.clone(), "e", 0).unwrap();
    assert_eq!((emb.dimensions(), emb.batch_size(), emb.model_ref().as_str()), (3, 2, "e@r1"));
    let texts: Vec<String> = ["a", "bb", "ccc", "dddd", "eeeee"].iter().map(|s| s.to_string()).collect();
    let vecs = emb.embed_texts(&texts).await.unwrap();
    assert_eq!(srv.hits(), 3, "5 texts in batches of 2");
    assert_eq!(vecs.iter().map(|v| v[0] as usize).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5], "input order kept");
    assert!(emb.embed_texts(&[]).await.unwrap().is_empty());

    // Missing `dimensions` is a configuration error.
    let c2 = client(&registry_toml("e2", "embedding", &[&srv.base_url], ""));
    assert!(matches!(OpenAiEmbedder::new(c2, "e2", 4), Err(ModelError::Config(_))));

    // Dimension mismatch is reported.
    let c3 = client(&registry_toml("e", "embedding", &[&srv.base_url], "dimensions = 8"));
    let emb3 = OpenAiEmbedder::new(c3, "e", 4).unwrap();
    assert!(matches!(emb3.embed_texts(&texts[..1]).await, Err(ModelError::Response(_))));
}

/// Interactive work goes to the reserved replicas and batch work never does.
///
/// The suite reserves page *slots* for interactive uploads on its own side, which stops a
/// batch from filling a copy and does nothing about the batch's requests already queued
/// at the GPU — the starvation `deploy/gpu` names. A model that declares
/// `interactive_endpoints` gets a replica set only interactive pages are sent to, and
/// this is the test that the lane is real: two servers, and which one saw the request.
#[tokio::test]
async fn an_interactive_request_takes_the_reserved_lane_and_a_batch_one_never_does() {
    let batch_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reserved_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let batch = MockServer::start({
        let n = batch_hits.clone();
        Arc::new(move |_, _| {
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (200, chat_ok("from the shared pool"))
        })
    })
    .await;
    let reserved = MockServer::start({
        let n = reserved_hits.clone();
        Arc::new(move |_, _| {
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (200, chat_ok("from the reserved pool"))
        })
    })
    .await;

    let toml = format!(
        "{}interactive_endpoints = [{:?}]\n",
        registry_toml("vlm", "generic_json", &[&batch.base_url], ""),
        reserved.base_url
    );
    let client = OpenAiClient::new(ModelRegistry::from_toml(&toml).expect("registry"));

    let req = |interactive: bool| ChatRequest {
        prompt: "p".into(),
        max_tokens: 8,
        interactive,
        ..Default::default()
    };
    let r = client.chat("vlm", &req(true)).await.expect("interactive");
    assert_eq!(r.text, "from the reserved pool");
    let r = client.chat("vlm", &req(false)).await.expect("batch");
    assert_eq!(r.text, "from the shared pool");
    // Several batch requests must not leak into the reserved set, which is the whole
    // point of reserving it.
    for _ in 0..5 {
        client.chat("vlm", &req(false)).await.expect("batch");
    }
    assert_eq!(reserved_hits.load(std::sync::atomic::Ordering::Relaxed), 1, "batch work reached the reserved replicas");
    assert_eq!(batch_hits.load(std::sync::atomic::Ordering::Relaxed), 6);

    // Embeddings are Stage 2 work on a whole document and are batch by nature, whatever
    // the upload's priority was.
    let embedder_toml = format!(
        "{}interactive_endpoints = [{:?}]\ndimensions = 3\n",
        registry_toml("embed", "generic_json", &[&batch.base_url], ""),
        reserved.base_url
    );
    let embedder = OpenAiClient::new(ModelRegistry::from_toml(&embedder_toml).expect("registry"));
    let _ = embedder.embeddings("embed", &["a".to_string()]).await;
    assert_eq!(reserved_hits.load(std::sync::atomic::Ordering::Relaxed), 1, "an embedding call took a reserved replica");
}

/// A model with no reserved set serves interactive work from the shared one — which is
/// every deployment with a single pool, and must not become an error.
#[tokio::test]
async fn without_a_reserved_set_interactive_work_uses_the_shared_one() {
    let srv = MockServer::start(Arc::new(|_, _| (200, chat_ok("ok")))).await;
    let client = OpenAiClient::new(ModelRegistry::from_toml(&registry_toml("vlm", "generic_json", &[&srv.base_url], "")).expect("registry"));
    let r = client
        .chat("vlm", &ChatRequest { prompt: "p".into(), max_tokens: 8, interactive: true, ..Default::default() })
        .await
        .expect("interactive on a single-pool model");
    assert_eq!(r.text, "ok");
}
