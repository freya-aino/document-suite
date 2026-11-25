//! A span, exported over OTLP, arrives at a collector.
//!
//! Its own binary because `telemetry::init` is once-per-process, and it stands up a real
//! socket rather than asserting the exporter was *configured*: the failure this guards
//! against is the one the stub had for four revisions — a setting that is read, logged
//! about, and sends nothing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ds_infra::config::TelemetryConfig;

/// What the fake collector saw: the request target and the body.
type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Accepts one OTLP/HTTP request and answers 200, recording what it got.
///
/// A real collector, not a mock of the exporter, because what is being tested is whether
/// bytes leave the process at all.
async fn collector() -> (u16, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let out = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                // Read headers, then exactly `content-length` bytes of body.
                let (head_end, len) = loop {
                    let mut chunk = [0u8; 4096];
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = find(&buf, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let mut chunk = [0u8; 4096];
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let target = String::from_utf8_lossy(&buf[..buf.iter().position(|b| *b == b'\r').unwrap_or(0)]).to_string();
                seen.lock().expect("lock").push((target, buf[head_end..].to_vec()));
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                let _ = sock.flush().await;
            });
        }
    });
    (port, out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_span_reaches_the_collector_and_carries_the_service_name() {
    let (port, seen) = collector().await;
    let cfg = TelemetryConfig {
        // No path: `init` appends `/v1/traces`, which is the OTLP/HTTP convention and the
        // thing a deployment gets wrong by configuring the gRPC port instead.
        otlp_endpoint: Some(format!("http://127.0.0.1:{port}")),
        metrics_addr: None,
        json_logs: false,
        // Every trace, which is what this test asserts reaches the collector; the
        // sampler itself is covered by `sampling_is_parent_based_and_clamped`.
        sample_ratio: 1.0,
    };
    let t = ds_infra::telemetry::init(&cfg, "ds-worker-traced").expect("init");

    // A span shaped like a real one: §5.5's standard fields on a worker task.
    tracing::info_span!("task", role = "page", job_id = "j-1", page_no = 7).in_scope(|| {
        tracing::info!("handled");
    });

    // Forces the batch processor to flush rather than waiting out its timer, which is
    // also exactly what a worker's shutdown has to do or it loses its last batch.
    t.shutdown();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = seen.lock().expect("lock").clone();
        if let Some((target, body)) = got.first() {
            assert!(target.starts_with("POST /v1/traces "), "wrong target: {target:?}");
            let text = String::from_utf8_lossy(body);
            assert!(text.contains("ds-worker-traced"), "the service name is not in the payload");
            assert!(text.contains("task"), "the span name is not in the payload");
            assert!(text.contains("job_id"), "the span fields are not in the payload");
            return;
        }
        assert!(std::time::Instant::now() < deadline, "no OTLP request arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
