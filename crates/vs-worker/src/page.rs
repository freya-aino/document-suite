//! Role `page` (Stage 1). See SPEC.md.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use vs_core::{Block, HashKey, PageProfile, PageResult, RunId};
use vs_dag::{BlockDraft, ExecOutput, StepRegistry};

/// All steps a production worker knows. DAG lists are validated against this.
pub fn build_step_registry(client: Arc<vs_models::OpenAiClient>) -> StepRegistry {
    let mut reg = StepRegistry::new();
    reg.register(Arc::new(vs_ingest::steps::MergeNative))
        .register(Arc::new(vs_ingest::steps::MergeBlocks))
        .register(Arc::new(vs_ingest::steps::FigureCrop));
    vs_models::register_steps(&mut reg, client);
    reg
}

/// Turns executor output into final blocks: stable index by reading order, hash keys,
/// provenance taken from the trace of the node that produced each draft.
pub fn drafts_to_blocks(out: ExecOutput, page_hk: &HashKey, run_id: &RunId) -> Vec<Block> {
    let prov: HashMap<&str, &vs_core::Provenance> = out.traces.iter().map(|t| (t.node_id.as_str(), &t.provenance)).collect();
    let mut drafts: Vec<BlockDraft> = out.blocks;
    drafts.sort_by_key(|d| d.reading_order);
    drafts
        .into_iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let p = prov.get(d.node_id.as_str())?;
            let index = i as u32;
            Some(Block {
                block_hk: HashKey::block(page_hk, run_id, index),
                index,
                kind: d.kind,
                bbox: d.bbox,
                reading_order: index,
                content: d.content,
                text: d.text,
                level: d.level,
                confidence: d.confidence,
                provenance: (*p).clone(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn page_result(
    document_hk: HashKey,
    page_no: u32,
    run_id: RunId,
    profile: PageProfile,
    dag_ref: String,
    width: u32,
    height: u32,
    blocks: Vec<Block>,
) -> PageResult {
    PageResult {
        schema_version: PageResult::SCHEMA_VERSION,
        page_hk: HashKey::page(&document_hk, page_no),
        document_hk,
        page_no,
        run_id,
        render_width_px: width,
        render_height_px: height,
        profile,
        dag_ref,
        blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vs_core::{BBox, BlockContent, BlockKind, Provenance};
    use vs_dag::{NodeOutcome, NodeTrace};

    #[test]
    fn drafts_get_keys_and_provenance() {
        let run = RunId::new();
        let page_hk = HashKey::of(&["p"]);
        let prov = Provenance {
            run_id: run,
            dag_ref: "d@1".into(),
            node_id: "parse".into(),
            step_kind: "page.parse_e2e".into(),
            model_ref: Some("m@1".into()),
            prompt_ref: None,
            params_hash: "0".into(),
            started_at: chrono::Utc::now(),
            duration_ms: 5,
        };
        let draft = |order: u32| BlockDraft {
            kind: BlockKind::Paragraph,
            bbox: BBox::new(0.0, 0.0, 1.0, 0.1).unwrap(),
            reading_order: order,
            content: BlockContent::Text { text: format!("b{order}") },
            text: format!("b{order}"),
            level: None,
            confidence: 0.9,
            node_id: "parse".into(),
        };
        let out = ExecOutput {
            blocks: vec![draft(5), draft(2)],
            traces: vec![NodeTrace { node_id: "parse".into(), provenance: prov, outcome: NodeOutcome::Ok }],
        };
        let blocks = drafts_to_blocks(out, &page_hk, &run);
        assert_eq!(blocks[0].text, "b2");
        assert_eq!(blocks[1].index, 1);
        assert_eq!(blocks[1].block_hk, HashKey::block(&page_hk, &run, 1));
        assert_eq!(blocks[0].provenance.model_ref.as_deref(), Some("m@1"));
    }
}
