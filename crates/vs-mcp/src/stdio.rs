//! The stdio transport: one JSON-RPC message per line, requests on stdin, replies on
//! stdout.
//!
//! **Nothing but protocol may touch stdout.** A log line there corrupts the stream and the
//! client's next parse fails on something that looks like a server bug, so this binary
//! sends every log to stderr in this mode — see `main.rs`, where that is arranged before
//! anything can log.
//!
//! Messages are handled **one at a time, in order**. A concurrent version would need the
//! client to tolerate out-of-order replies (it must, by id) and would buy nothing here:
//! the tools are I/O-bound against one API, an agent sends one call at a time, and a
//! serial loop means a slow `wait_for_job` cannot interleave its output with another
//! reply's.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::jsonrpc;
use crate::server::Server;

/// Reads until stdin closes, which is how a client says the session is over.
pub async fn serve(server: Arc<Server>) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();
    tracing::info!(api = server.suite.base(), "mcp server on stdio");

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        // Blank lines are not messages. Some clients write them as keep-alives, and
        // answering one with a parse error would be noise in both directions.
        if line.is_empty() {
            continue;
        }
        let reply = match jsonrpc::parse(line) {
            Ok(msg) => server.handle(msg, None).await,
            Err(e) => {
                // The id cannot be trusted from a message that did not parse, so this
                // answers with a null id, which is what JSON-RPC prescribes.
                Some(crate::server::refuse(None, &e))
            }
        };
        if let Some(reply) = reply {
            write_line(&mut stdout, &reply).await?;
        }
    }
    tracing::info!("stdin closed; stopping");
    Ok(())
}

/// One message, one line, flushed.
///
/// Flushed every time on purpose: stdout to a pipe is block-buffered, so an unflushed
/// reply sits in the buffer while the client waits for it and the session deadlocks over
/// nothing.
async fn write_line(out: &mut tokio::io::Stdout, msg: &crate::jsonrpc::Outgoing) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    out.write_all(&line).await?;
    out.flush().await?;
    Ok(())
}
