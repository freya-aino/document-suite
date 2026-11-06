//! Pool of render child processes (see [`crate::child`]). Each child owns one pdfium
//! instance, so pages render in parallel, and a hang or crash costs one child: hangs
//! are killed after a timeout, crashes are retried once on a fresh child.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Semaphore;

use crate::child::{page_assets, Reply, Request};
use crate::format::Format;
use crate::raster::{IngestError, PageAssets};

/// Upper bound for one reply frame (a page PNG at the 12 000 px cap is far below this).
const MAX_FRAME: usize = 1 << 30;

/// How to start a child, e.g. the worker binary with `render-child`.
#[derive(Debug, Clone)]
pub struct ChildCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Concurrent children (default: available CPUs).
    pub size: usize,
    pub page_timeout: Duration,
    /// Opening a document to count pages.
    pub open_timeout: Duration,
}

pub struct RenderPool {
    cmd: ChildCommand,
    cfg: PoolConfig,
    permits: Semaphore,
    idle: Mutex<Vec<ChildProc>>,
}

struct ChildProc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl RenderPool {
    pub fn new(cmd: ChildCommand, cfg: PoolConfig) -> Self {
        let size = cfg.size.max(1);
        Self { cmd, cfg: PoolConfig { size, ..cfg }, permits: Semaphore::new(size), idle: Mutex::new(Vec::new()) }
    }

    pub fn size(&self) -> usize {
        self.cfg.size
    }

    pub async fn page_count(&self, path: &Path, format: Format, max_pages: u32) -> Result<u32, IngestError> {
        let req = Request::Count { path: path.to_path_buf(), format, max_pages };
        match self.call(&req, self.cfg.open_timeout, 0).await? {
            (Reply::Count { page_count }, _) => Ok(page_count),
            (other, _) => Err(unexpected(&other)),
        }
    }

    pub async fn page(&self, path: &Path, format: Format, max_pages: u32, page_no: u32, dpi: u32, thumb_long_side_px: u32) -> Result<PageAssets, IngestError> {
        let req = Request::Page { path: path.to_path_buf(), format, max_pages, page_no, dpi, thumb_long_side_px };
        match self.call(&req, self.cfg.page_timeout, page_no).await? {
            (Reply::Page { image, thumbnail, text_layer, signals }, mut payloads) if payloads.len() == 2 => {
                let jpeg = payloads.pop().unwrap_or_default();
                let png = payloads.pop().unwrap_or_default();
                Ok(page_assets(image, thumbnail, text_layer, signals, png, jpeg))
            }
            (other, _) => Err(unexpected(&other)),
        }
    }

    async fn call(&self, req: &Request, timeout: Duration, page: u32) -> Result<(Reply, Vec<Vec<u8>>), IngestError> {
        let _permit = self.permits.acquire().await.map_err(|_| IngestError::Io("render pool closed".into()))?;
        let mut crash = String::new();
        for attempt in 1..=2 {
            let popped = self.idle.lock().unwrap_or_else(|p| p.into_inner()).pop();
            let mut c = match popped {
                Some(c) => c,
                None => self.spawn()?,
            };
            match tokio::time::timeout(timeout, exchange(&mut c, req)).await {
                Ok(Ok((Reply::Error { error }, _))) => {
                    self.idle.lock().unwrap_or_else(|p| p.into_inner()).push(c);
                    return Err(error.into());
                }
                Ok(Ok(reply)) => {
                    self.idle.lock().unwrap_or_else(|p| p.into_inner()).push(c);
                    return Ok(reply);
                }
                Ok(Err(e)) => {
                    crash = format!("{e}; {}", exit_status(&mut c).await);
                    tracing::warn!(page, attempt, reason = %crash, "render child crashed");
                }
                Err(_) => {
                    let _ = c.child.kill().await;
                    tracing::warn!(page, ?timeout, "render child timed out, killed");
                    return Err(IngestError::Timeout { page });
                }
            }
        }
        Err(IngestError::Crashed { page, reason: crash })
    }

    fn spawn(&self) -> Result<ChildProc, IngestError> {
        let mut child = Command::new(&self.cmd.program)
            .args(&self.cmd.args)
            .envs(self.cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| IngestError::Io(format!("spawning render child {:?}: {e}", self.cmd.program)))?;
        let stdin = child.stdin.take().ok_or_else(|| IngestError::Io("render child stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| IngestError::Io("render child stdout".into()))?;
        Ok(ChildProc { child, stdin, stdout: BufReader::new(stdout) })
    }
}

async fn exchange(c: &mut ChildProc, req: &Request) -> std::io::Result<(Reply, Vec<Vec<u8>>)> {
    let mut line = serde_json::to_vec(req).map_err(std::io::Error::other)?;
    line.push(b'\n');
    c.stdin.write_all(&line).await?;
    c.stdin.flush().await?;
    let reply: Reply = serde_json::from_slice(&read_frame(&mut c.stdout).await?).map_err(std::io::Error::other)?;
    let payloads = match reply {
        Reply::Page { .. } => vec![read_frame(&mut c.stdout).await?, read_frame(&mut c.stdout).await?],
        _ => vec![],
    };
    Ok((reply, payloads))
}

async fn read_frame(r: &mut BufReader<ChildStdout>) -> std::io::Result<Vec<u8>> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("frame of {len} bytes")));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn exit_status(c: &mut ChildProc) -> String {
    match tokio::time::timeout(Duration::from_secs(2), c.child.wait()).await {
        Ok(Ok(status)) => format!("child exited: {status}"),
        _ => {
            let _ = c.child.kill().await;
            "child killed".into()
        }
    }
}

fn unexpected(reply: &Reply) -> IngestError {
    IngestError::Io(format!("unexpected render child reply: {reply:?}"))
}
