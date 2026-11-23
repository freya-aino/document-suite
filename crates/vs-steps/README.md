# vs-steps

The production step catalogue: one place that names every step a `page` worker can run.
The worker roles execute from it, the API validates uploaded DAG lists against it, and
`vs-adapt` generates candidates from it — so all three agree on what exists. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md). Step contract:
[`vs-dag/README.md`](../vs-dag/README.md) "Step contract".

## What it does

`production_registry(client)` returns the 17 steps a worker knows: 4 CPU steps and 6
pre-processing steps from `vs-ingest`, and 7 model-backed steps from `vs-models`. Tests
assert the registry holds **exactly** the documented catalogue — neither more nor fewer
kinds — that the shipped example DAG list loads against it, and that every template in
the example search space generates candidates it accepts.

## Known limitations

- `page.dewarp` is not implemented; see `vs-ingest`'s status for why a page-curl warp
  needs the page boundary rather than ink statistics. `page.orient` *is* registered, and
  its own documentation separates the half that is measured from the half that is
  judged.

## The one rule

**Add new steps here, nowhere else.** A step that is implemented but not registered is
invisible to DAG-list validation, so a DAG naming it passes upload validation in
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
| `page.binarize` | CPU | `vs_ingest::preprocess::Binarize` — global Otsu, or Sauvola per neighbourhood with `adaptive = true` |
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
