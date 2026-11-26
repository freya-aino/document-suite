//! The fake model tier as a service, for the `fake` tier of `deploy/compose`.
//!
//! The same `ds_testkit::models::router()` the unit tests serve in-process, so the model
//! tier a pull request exercises is the code the tests exercise. It answers on whatever
//! service name it is reached by — the compose overlay gives one container all six
//! registry hostnames as aliases, because dispatch is on the prompt, not the host.
//!
//! It serves **functionality, not fidelity**: the answers are ours, so a quality number
//! measured against this process is a measurement of this process.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("DS_FAKE_MODELS_ADDR").unwrap_or_else(|_| "0.0.0.0:8000".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("ds-fake-models listening on {addr}");
    axum::serve(listener, ds_testkit::models::router()).await?;
    Ok(())
}
