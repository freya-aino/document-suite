//! Role `doc` (Stage 2). See SPEC.md.

#![allow(dead_code)]

use vs_core::{PageResult, Profile};
use vs_extract::Extractor;

/// Handles `Task::Doc` up to the point where stores are written.
pub async fn process(
    pages: Vec<PageResult>,
    pages_total: u32,
    profile: Profile,
    extractor: &dyn Extractor,
) -> anyhow::Result<(vs_stitch::StitchedDocument, vs_extract::ExtractionOutput, vs_graph::DocGraph)> {
    let stitched = vs_stitch::stitch(pages, pages_total)?;
    let ext = extractor.extract(&stitched, profile).await?;
    // corpus resolution (HelixResolver) sets entity_hk on ext.entities here
    let title = stitched
        .blocks
        .iter()
        .find(|b| b.block.kind == vs_core::BlockKind::Title)
        .map(|b| b.block.text.clone());
    let graph = vs_graph::build(&stitched, &ext, title.as_deref());
    graph.validate().map_err(|e| anyhow::anyhow!("graph invariant violated: {e:?}"))?;
    // embed graph.embed_targets → Vectors; VaultWriter::write_stage2; GraphStore::write_run
    Ok((stitched, ext, graph))
}
