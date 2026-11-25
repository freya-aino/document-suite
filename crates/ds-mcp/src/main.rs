//! The `ds-mcp` binary: argument parsing, logging, and whichever transport was asked for.
//!
//! The protocol and the tools live in the library beside this file (`lib.rs`), which is
//! what the tests drive. This file is the part that only a process has: a command line, a
//! log destination, and a signal handler.

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use ds_mcp::server::Server;
use ds_mcp::suite::{Suite, TokenSource};
use ds_mcp::tools::Limits;
use ds_mcp::{http, stdio};

#[derive(Parser)]
#[command(about = "Model Context Protocol server for the document suite", version)]
struct Args {
    /// Base URL of the suite's HTTP API.
    #[arg(long, env = "DS_API_URL", default_value = "http://127.0.0.1:8080", global = true)]
    api: String,
    /// Seconds one API call may take. A `wait_for_job` poll is one call, not the whole
    /// wait.
    #[arg(long, env = "DS_MCP_API_TIMEOUT", default_value_t = 60, global = true)]
    api_timeout: u64,
    /// Characters of document text one `get_document_text` call may return.
    #[arg(long, env = "DS_MCP_MAX_TEXT_CHARS", default_value_t = 60_000, global = true)]
    max_text_chars: usize,
    /// Pages one `get_document_text` call may fetch.
    #[arg(long, env = "DS_MCP_MAX_PAGES", default_value_t = 20, global = true)]
    max_pages: u32,
    /// Longest `wait_for_job` may wait, in seconds.
    #[arg(long, env = "DS_MCP_MAX_WAIT", default_value_t = 120, global = true)]
    max_wait: u64,
    /// Seconds between polls in `wait_for_job`.
    #[arg(long, env = "DS_MCP_POLL_INTERVAL", default_value_t = 3, global = true)]
    poll_interval: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One session on stdin/stdout, for a client that starts this process per user.
    ///
    /// The token is the **user's own**, from `DS_API_TOKEN` or `--token-file`: the process
    /// belongs to them, so its environment does (decision D16).
    Stdio {
        /// A file holding the bearer token, read once at startup. Preferred over an
        /// environment variable on a shared machine, where `/proc` makes an environment
        /// readable.
        #[arg(long, env = "DS_API_TOKEN_FILE")]
        token_file: Option<std::path::PathBuf>,
    },
    /// A long-running service, where every request carries its own caller's token.
    Http {
        #[arg(long, env = "DS_MCP_ADDR", default_value = "127.0.0.1:8081")]
        addr: String,
        /// A browser origin that may call this server. Repeatable. Without any, a request
        /// carrying an `Origin` header is refused — which is what stops a web page from
        /// using a localhost server as the user.
        #[arg(long = "allowed-origin")]
        allowed_origins: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let limits = Limits {
        max_text_chars: args.max_text_chars.max(1_000),
        max_pages: args.max_pages.max(1),
        max_wait: Duration::from_secs(args.max_wait.max(1)),
        poll_interval: Duration::from_secs(args.poll_interval.clamp(1, 60)),
    };

    match args.cmd {
        Cmd::Stdio { token_file } => {
            // **Logs to stderr, before anything can log.** stdout carries the protocol in
            // this mode, and a log line on it corrupts the stream — the client's next
            // parse then fails on what looks like a server bug. This is also why
            // `ds_infra::telemetry` is not used here: it writes to stdout, correctly, for
            // every other binary in the workspace.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
                .init();

            let token = read_token(token_file.as_deref())?;
            if matches!(token, TokenSource::None) {
                // Not fatal: `tools/list` works without a token, and a client that starts
                // this process to look at the catalogue should not be met with an exit.
                // Every *call* then fails with a message saying exactly this.
                tracing::warn!("no DS_API_TOKEN and no --token-file: tool calls will fail until one is supplied");
            }
            let server = Arc::new(Server { suite: Suite::new(&args.api, token, Duration::from_secs(args.api_timeout))?, limits });
            stdio::serve(server).await
        }
        Cmd::Http { addr, allowed_origins } => {
            let telemetry = ds_infra::telemetry::init(&Default::default(), "ds-mcp")?;
            // In HTTP mode the token is per request and this process holds none at all,
            // which is the property worth stating in the log: an operator reading it can
            // see that the service cannot act on its own.
            let server = Arc::new(Server { suite: Suite::new(&args.api, TokenSource::PerRequest, Duration::from_secs(args.api_timeout))?, limits });
            let state = http::HttpState { server: server.clone(), allowed_origins: Arc::new(allowed_origins) };
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!(
                %addr,
                api = server.suite.base(),
                origins = state.allowed_origins.len(),
                "mcp server on http; every call uses its own caller's token"
            );
            let serving = axum::serve(listener, http::router(state)).with_graceful_shutdown(shutdown());
            serving.await?;
            telemetry.shutdown();
            Ok(())
        }
    }
}

/// The token for a stdio session: a file if one was named, else `DS_API_TOKEN`.
///
/// A file is the better of the two on a shared machine — an environment is readable
/// through `/proc` by anyone who can read the process — and neither is this service's own
/// credential: both are the user's, which is the whole of decision D16.
fn read_token(path: Option<&std::path::Path>) -> anyhow::Result<TokenSource> {
    if let Some(path) = path {
        let token = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let token = token.trim();
        if token.is_empty() {
            anyhow::bail!("{} is empty", path.display());
        }
        return Ok(TokenSource::Fixed(token.into()));
    }
    match std::env::var("DS_API_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Ok(TokenSource::Fixed(t.trim().into())),
        _ => Ok(TokenSource::None),
    }
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
