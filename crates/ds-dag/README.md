# ds-dag

Stage 1 workflows. A **DAG** describes how one page is turned into blocks: which steps
run, in what order, with which parameters. DAGs are static data (JSON); they are produced
by `ds-adapt` and selected per page by `ds-router`. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) (reliability contract: §5.4).

## What it does

Holds the DAG model, load-time validation with unreachable-node pruning, the
level-parallel executor with per-node timeouts and optional-node fallback, the artefact
kinds steps exchange, the `PageSource` capability a step uses to ask for a finer render of
its page, and the `NodeTrace` that records what each node did and what it reported about
doing it.

## Known limitations

- Every kind in the catalogue below is implemented and registered, but `blocks.verify`
  is the CPU half only: it compares two parsers' readings arithmetically. The `CPU/GPU`
  tier the catalogue once claimed for it anticipated a VLM adjudicating a disagreement,
  which is not built — see `ds-stitch`'s post-v1 note for why that stays out of v1.
- No artefact kind carries region relationships; `LayoutRegion` has no id and no
  parent, so a step cannot express that one region contains another.
- `StepContext::pages` is the only capability a step has beyond its inputs, and it is
  deliberately narrow: "this page at this DPI", for one document, with no way to name
  another. A step that needed the *original bytes* — to read embedded attachments, say —
  would need a second capability rather than a wider version of this one.

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
- A step may also report **`StepOutput::metrics`**: a handful of named scalars about *how*
  it produced the output, which travel into the `NodeTrace` and from there to `ds-adapt`
  and the vault. It exists because of one concrete case — the `auto` output family tries
  several parsers and which one won is the single most useful fact about the call, and
  there was nowhere to put it, so it went to a `tracing` target an operator could read and
  the scorer could not. Never the output itself: this is metadata, and a step putting its
  blocks here would store them twice.
- A step may ask `ctx.pages` for its page at a finer DPI (`crop_dpi`). It is `None` in an
  offline run and wherever a deployment disables re-rendering, so a step **must** fall back
  to the page image it was given rather than fail.
- Step implementations live in `ds-ingest` (CPU steps) and `ds-models` (model steps), and
  are registered in a `StepRegistry` at worker start.

### Pass-through on failure (`page_image → page_image` steps)

A step whose output kind is `PageImage` cannot be `optional` in the usual way, because
`Artifact::empty` has no empty form for a page image — and rightly so: an empty page is
not a usable input to anything. Such a step must instead **return its input unchanged**
when it cannot do its work, and log why.

This is what makes the pre-processing steps safe to wire into a DAG speculatively: a step
that cannot improve a page must not be able to destroy it. A model can still read an
unconditioned page; it cannot read nothing.

## Validation (`Dag::validate`)

Run when a DAG list is loaded (worker start, registry change) and by `ds-adapt` before
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
  outcome, and whatever the step reported in `metrics`). The worker turns drafts into
  `ds_core::Block` with hash keys.
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
| `blocks.verify` | CPU | re-scores one block set by how far a second reading corroborates it; adds and drops nothing |
| `page.orient` | CPU | turns a sideways or upside-down page (quarter turns are measured, the flip is a gated judgement) |
| `page.deskew` | CPU | straightens a crooked scan (projection profile) |
| `page.denoise` | CPU | median filter for speckled scans and photos |
| `page.normalize` | CPU | contrast normalisation for faint or unevenly lit pages |
| `page.binarize` | CPU | Otsu threshold; **outputs greyscale** |
| `page.crop_margins` | CPU | trims scanner borders and blank margins |

## Metrics

`dag_node_duration_seconds{step,dag}`, `dag_node_failures_total{step,dag,reason}`,
`dag_pages_total{dag}`.
