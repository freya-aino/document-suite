//! `telemetry::init` installs one recorder and, when asked, serves it over HTTP.
//!
//! One test, in its own binary, because the thing under test is process-global: the
//! tracing subscriber and `metrics::set_global_recorder` can each be set once, and a
//! second `init` in the same process is supposed to fail rather than quietly produce a
//! second set of series.

use ds_infra::config::TelemetryConfig;

/// A port nothing is listening on, found by letting the OS pick one and giving it back.
///
/// Racy in principle. The alternative is a fixed port, which on a developer machine
/// running several of these projects at once is worse than racy — it is reliably taken.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    port
}

#[tokio::test]
async fn the_worker_endpoint_serves_what_the_workspace_records() {
    let port = free_port();
    let cfg = TelemetryConfig {
        otlp_endpoint: None,
        metrics_addr: Some(format!("127.0.0.1:{port}")),
        json_logs: false,
        sample_ratio: 1.0,
    };
    let t = ds_infra::telemetry::init(&cfg, "ds-worker-test").expect("init");

    metrics::counter!("worker_tasks_total", "role" => "page", "outcome" => "ok").increment(3);
    metrics::histogram!("worker_task_duration_seconds", "role" => "page").record(0.42);

    let body = reqwest::get(format!("http://127.0.0.1:{port}/metrics")).await.expect("scrape").text().await.expect("body");

    assert!(body.contains(r#"worker_tasks_total{service="ds-worker-test",role="page",outcome="ok"} 3"#), "{body}");
    // The global label is the whole reason a scrape of three worker roles is readable.
    assert!(body.contains(r#"service="ds-worker-test""#), "{body}");
    // The bucket set has to reach past the exporter's 10 s default: a Stage 2 document is
    // slower than that, and with the default buckets every such duration lands in `+Inf`.
    assert!(body.contains("worker_task_duration_seconds_bucket"), "{body}");
    assert!(body.contains(r#"le="600""#), "{body}");

    // The same registry the HTTP listener serves, for a binary that renders it itself.
    assert!(t.metrics.render().contains("worker_tasks_total"));

    // A second init in this process must fail, not shadow the first and not panic from
    // inside the subscriber as it used to.
    let Err(err) = ds_infra::telemetry::init(&cfg, "ds-worker-again") else { panic!("a second recorder was installed") };
    assert!(err.to_string().contains("already installed"), "{err}");
}
