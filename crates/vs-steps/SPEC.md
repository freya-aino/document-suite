# vs-steps — Specification

The production step catalogue: one place that names every step a `page` worker can run.
`vs-worker` executes from it, `vs-api` validates uploaded DAG lists against it, and
`vs-adapt` generates candidates from it — so all three agree on what exists. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md). Step contract:
[`vs-dag/SPEC.md`](../vs-dag/SPEC.md) "Step contract".

## Status

Implemented: `production_registry(client)` registering 15 steps — 3 CPU steps and 5
pre-processing steps from `vs-ingest`, and 7 model-backed steps from `vs-models`. Tests
assert the registry covers the documented catalogue, that the shipped example DAG list
loads against it, and that every template in the example search space generates
candidates it accepts.

Remaining:
- [ ] `blocks.verify` is in the `vs-dag` catalogue but has no implementation to register,
      so the registry covers 15 of 16 documented kinds. See `vs-dag`'s status for what is
      actually missing.
- [ ] `page.orient` and `page.dewarp` are not implemented; see `vs-ingest`'s status for
      why neither is a pixel-statistics problem.

## The one rule

**Add new steps here, nowhere else.** A step that is implemented but not registered is
invisible to DAG-list validation, so a DAG naming it passes `vs-api` upload validation in
one deployment and fails in another. A step registered here but not implemented is worse:
validation accepts DAG lists that fail at execution time, per page, after the model spend.

This is why `vs-adapt` builds its candidate generator from this same function rather than
its own list — a candidate it cannot validate here is a candidate production cannot run.

## Contents

| Step | Tier | Source |
|------|------|--------|
| `blocks.merge_native` | CPU | `vs_ingest::steps::MergeNative` |
| `blocks.merge` | CPU | `vs_ingest::steps::MergeBlocks` |
| `figure.crop` | CPU | `vs_ingest::steps::FigureCrop` |
| `page.deskew` | CPU | `vs_ingest::preprocess::Deskew` |
| `page.denoise` | CPU | `vs_ingest::preprocess::Denoise` |
| `page.normalize` | CPU | `vs_ingest::preprocess::Normalize` |
| `page.binarize` | CPU | `vs_ingest::preprocess::Binarize` |
| `page.crop_margins` | CPU | `vs_ingest::preprocess::CropMargins` |
| `page.parse_e2e` | GPU | `vs_models::steps::PageParseE2e` |
| `layout.detect` | GPU | `vs_models::steps::LayoutDetect` |
| `region.ocr` | GPU | `vs_models::steps::RegionStep::ocr` |
| `table.recognize` | GPU | `vs_models::steps::RegionStep::table` |
| `formula.recognize` | GPU | `vs_models::steps::RegionStep::formula` |
| `figure.describe` | GPU | `vs_models::steps::FigureDescribe` |
| `chart.to_table` | GPU | `vs_models::steps::ChartToTable` |

CPU steps take no model client. Model-backed steps share one `Arc<OpenAiClient>`, so the
per-model concurrency semaphore is shared across every step that uses that model — which
is what keeps a worker from flooding one GPU pool through two different steps at once.

## Tests

`tests/example_dag_list.rs` loads `config/dag-list.example.json` through
`vs_router::Router::load` against this registry. That test is the guard that the shipped
example DAG list stays runnable; it fails if a step is renamed or removed.
