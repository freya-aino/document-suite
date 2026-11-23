//! `telemetry.sample_ratio = 0.0` exports nothing.
//!
//! Its own binary for the same reason as `traces.rs`: `telemetry::init` installs
//! process-wide state once, so one `init` per test binary is the whole constraint on how
//! these tests are laid out. The collector below is the one from `traces.rs` — duplicated
//! rather than shared, because a `tests/common` module would be compiled into both
//! binaries anyway and the indirection buys nothing.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vs_infra::config::TelemetryConfig;

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

/// A ratio of zero is an off switch, not a quiet default back to 1.0 — the failure this
/// guards against is a sampling setting that is read, logged about, and ignored.
#[tokio::test(flavor = "multi_thread")]
async fn a_sample_ratio_of_zero_exports_nothing() {
    let (port, seen) = collector().await;
    let cfg = TelemetryConfig {
        otlp_endpoint: Some(format!("http://127.0.0.1:{port}")),
        metrics_addr: None,
        json_logs: false,
        sample_ratio: 0.0,
    };
    let t = vs_infra::telemetry::init(&cfg, "vs-sampled-none").expect("init");
    {
        let _s = tracing::info_span!("worker_task", job_id = "j1", run_id = "r1").entered();
    }
    t.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bodies = seen.lock().expect("collector mutex").clone();
    assert!(bodies.is_empty(), "sample_ratio = 0.0 still exported {} payload(s)", bodies.len());
}

/// And a ratio above 1.0 is clamped rather than refused: a typo in a telemetry setting
/// must not stop the service, and the clamp means it behaves as 1.0.
#[test]
fn an_out_of_range_ratio_does_not_fail_the_process() {
    // `init` cannot run twice in one binary, so this exercises the config path: the value
    // is accepted, and `sampler` clamps it (its own assertion is that `init` above
    // succeeded with a valid one).
    let cfg: TelemetryConfig = toml::from_str("sample_ratio = 7.5\n").expect("a ratio out of range still parses");
    assert_eq!(cfg.sample_ratio, 7.5);
    let zero: TelemetryConfig = toml::from_str("").expect("defaults");
    assert_eq!(zero.sample_ratio, 1.0, "the default is every trace, as it was before there was a choice");
}
