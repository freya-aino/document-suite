# vs-dag — Specification

Stage 1 workflows. A **DAG** describes how one page is turned into blocks: which steps
run, in what order, with which parameters. DAGs are static data (JSON); they are produced
by `vs-adapt` and selected per page by `vs-router`.

## Data model

```jsonc
{
  "id": "born-digital-tables",
  "version": "3",
  "description": "Native text + table model; no OCR",
  "output": "merge",                       // node whose artefact is the page's Blocks
  "nodes": [
    { "id": "layout", "step": "layout.detect",  "inputs": ["$page_image"],
      "params": { "model": "pp-doclayout-v3" } },
    { "id": "tables", "step": "table.recognize", "inputs": ["$page_image", "layout"],
      "params": { "model": "paddleocr-vl-1.5" }, "timeout_ms": 30000 },
    { "id": "merge",  "step": "blocks.merge_native", "inputs": ["$text_layer", "layout", "tables"] }
  ]
}
```

- `inputs` reference other node ids or **sources**: `$page_image`, `$text_layer`.
- `params` are opaque to the executor and validated by the step.
- `timeout_ms` overrides the step default. `optional: true` means a failure of this node
  does not fail the page; dependants receive an empty artefact of the right kind.

## Artefact kinds

| Kind | Payload | Produced by (examples) |
|------|---------|------------------------|
| `page_image` | rendered page bytes + size | source |
| `text_layer` | native text spans with bboxes | source (may be empty) |
| `layout` | regions: kind, bbox, reading order, score | `layout.detect` |
| `blocks` | block drafts (kind, bbox, order, content, text, confidence) | `page.parse_e2e`, `region.ocr`, `table.recognize`, `blocks.merge*` |
| `figure_crops` | cropped figure/chart images | `figure.crop` |

## Step contract

```rust
#[async_trait]
pub trait Step: Send + Sync {
    fn kind(&self) -> &'static str;                // e.g. "table.recognize"
    fn signature(&self) -> Signature;              // input kinds (ordered) + output kind
    fn default_timeout_ms(&self) -> u64;
    async fn run(&self, ctx: &StepContext, inputs: &[&Artifact], params: &Value)
        -> Result<StepOutput, StepError>;
}
```

- A step must be **pure with respect to its inputs and params** (same inputs → equivalent
  output), apart from model non-determinism. Model steps use temperature 0.
- A step reports `model_ref` and `prompt_ref` in `StepOutput`; the executor builds the
  `Provenance` (it owns timing, run id, node id and params hash).
- Step implementations live in `vs-ingest` (CPU steps) and `vs-models` (model steps), and
  are registered in a `StepRegistry` at worker start.

## Validation (`Dag::validate`)

Run when a DAG list is loaded (worker start, registry change) and by `vs-adapt` before
scoring. A DAG is rejected if:

1. node ids are not unique or start with `$`;
2. an input refers to an unknown node or source;
3. there is a cycle;
4. `output` is missing or its step does not output `blocks`;
5. a step kind is unknown to the registry;
6. input count or kinds do not match the step signature.

Validation returns the execution **levels** (nodes whose inputs are all satisfied by
earlier levels). Nodes in one level run concurrently.

## Executor

- Runs levels in order; within a level, nodes run concurrently (`join_all`).
- Each node runs under `tokio::time::timeout`.
- Failure of a non-optional node fails the page with `ExecError::Node { node_id, .. }`.
  Retrying is the worker's job (whole page), not the executor's.
- Output: final `Vec<BlockDraft>` plus a `NodeTrace` per node (provenance, duration,
  outcome). The worker turns drafts into `vs_core::Block` with hash keys.
- Unused branches: nodes not on a path to `output` are skipped (pruned at validation).

## Step catalogue (v1 kinds)

| Kind | Tier | Notes |
|------|------|-------|
| `layout.detect` | GPU | layout regions + reading order |
| `page.parse_e2e` | GPU | one VLM call → blocks with content (e.g. PaddleOCR-VL, MinerU, dots.ocr) |
| `region.ocr` | GPU | per-region text recognition (incl. handwriting models) |
| `table.recognize` | GPU | table regions → HTML |
| `formula.recognize` | GPU | formula regions → LaTeX |
| `figure.crop` | CPU | crops figure/chart regions |
| `figure.describe` | GPU | general VLM description of crops |
| `chart.to_table` | GPU | chart crops → data table HTML |
| `blocks.merge_native` | CPU | native text layer + layout (+ recognised tables/formulas) → blocks |
| `blocks.merge` | CPU | merge several block sets (union by IoU, keeps highest-confidence) |
| `blocks.verify` | CPU/GPU | compares two block sets; sets confidence from agreement |

## Metrics

`dag_node_duration_ms{step,dag}`, `dag_node_failures_total{step,dag,reason}`,
`dag_pages_total{dag}`.
