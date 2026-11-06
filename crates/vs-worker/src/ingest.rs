//! Role `ingest`: see vs-ingest/SPEC.md "Ingest flow".

#![allow(dead_code)]

use vs_core::{content_sha256, HashKey, JobSpec, RunId};

/// Handles `Task::Ingest`; returns the page count.
pub async fn handle(job_id: uuid::Uuid, run_id: RunId, spec: &JobSpec, original: Vec<u8>) -> anyhow::Result<u32> {
    let sha = content_sha256(&original);
    let document_hk = HashKey::document(&spec.tenant, &sha);
    let format = vs_ingest::detect_format(&original).ok_or_else(|| anyhow::anyhow!("unsupported_format"))?;
    tracing::info!(%job_id, %run_id, %document_hk, ?format, "ingesting");
    // open rasterizer (spawn_blocking) → per page: text layer, signals, render, thumbnail → blob
    // VaultWriter::write_document; insert ops.page_tasks; publish Task::Page per page
    todo!("ingest flow")
}
