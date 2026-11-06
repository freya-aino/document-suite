# vs-adapt — Specification

Runs in **dev/stage only**. Given datasets of documents, it generates candidate Stage 1
DAGs, runs them, scores them per page class, and emits a **DAG list** (DAGs + routing
table) to promote to production. This is where the "best performing workflow per
language/content" knowledge is produced (G3).

```
train → DAG list → prod upload → classification → selected DAG
```

## Commands

| Command | Input | Output |
|---------|-------|--------|
| `vs-adapt generate` | `search_space.json`, step catalogue (from the worker's StepRegistry) | `candidates.json` (validated DAGs) |
| `vs-adapt run` | dataset manifest, candidates, stage cluster config | page results per (page, DAG) in the blob store + `runs.jsonl` (latency, GPU-seconds, tokens) |
| `vs-adapt score` | runs, ground truth (optional), judge model config | `evals.jsonl` (`PageEval` per page × DAG) |
| `vs-adapt select` | evals, objective, candidates | `dag-list.json` + `report.md` |
| `vs-adapt promote` | dag-list.json | uploads as `candidate` via `POST /v1/dag-lists` (activation stays a manual step) |

## Datasets

Manifest (JSONL), one line per document:
`{ uri, language, doc_type_hint?, ground_truth_uri?, split: "train"|"holdout" }`.
Ground truth format: per page, blocks with kind, bbox, text / table HTML / LaTeX,
reading order (OmniDocBench-like). Public sets to seed: OmniDocBench, PureDocBench,
ParseBench, Dr. DocBench, plus your own documents.

All pages are classified with the production classifier first, so scores are grouped by
the **same `PageProfile` the router will see** in production (classifier errors are part
of the measurement, not hidden).

## Candidate generation

`search_space.json` lists **templates** — DAG skeletons with parameter slots — and the
values each slot can take:

```jsonc
{
  "templates": [
    { "id": "e2e", "dag": { /* nodes with "{model}" slots */ },
      "slots": { "model": ["paddleocr-vl-1.5", "mineru-2.5", "dots-ocr"] } },
    { "id": "native+layout+tables", "dag": { … },
      "slots": { "layout_model": ["pp-doclayout-v3"], "table_model": ["paddleocr-vl-1.5", "mineru-2.5"] } }
  ],
  "max_candidates": 200
}
```

Cartesian product per template, capped by `max_candidates`; every candidate must pass
`Dag::validate`. Candidate id = `<template>/<slot values>`; version = content hash.
Templates encode the "skip" structure (e.g. text-layer DAGs without OCR), so the search
explores cheap DAGs as well as robust ones.

To control cost, `run` uses **successive halving**: all candidates on a small sample per
class, keep the top third by utility, double the sample, repeat until the holdout size.

## Scoring

Per page × DAG (`PageEval`):

| Metric | Measure |
|--------|---------|
| text | 1 − normalised edit distance on reading-order text |
| tables | TEDS |
| formulas | CDM or normalised LaTeX edit distance |
| reading order | 1 − normalised edit distance on block order |
| layout | F1 at IoU 0.5 by kind |
| **quality** | weighted mean of the applicable metrics (weights in config; metrics that do not apply to the page are left out) |
| latency_ms | wall time of the DAG for this page |
| gpu_seconds | sum over model calls (from server metrics or token-based estimate) |
| trust | `labelled` (ground truth) or `judge` (no labels: agreement with the best-quality DAG plus a judge VLM rating, assumption A15) |

Judge-only classes are marked in the report; they can be excluded from promotion.

## Objective and selection

```
utility(dag, class, profile) =
    if quality < quality_floor: excluded
    else w_q · quality − w_l · latency/latency_ref − w_c · gpu_seconds/gpu_ref
```

Weights per caller profile (`quality`, `balanced`, `fast`); defaults in `Objective::default()`.

**Classes with back-off.** A class key is the discretised profile. Rules are produced at
several specificity levels by dropping conditions in this order until a class has at
least `min_pages` evaluated pages:

`has_charts → has_images → has_formulas → has_tables → has_handwriting → born_digital → doc_type → language`

For every (class, profile) with enough pages the best-utility DAG becomes a rule with
`score = utility`. A rule is **pruned** if the next less specific class picks the same DAG
(it adds nothing). The router's most-specific-match rule then reproduces this back-off.

- `default_dag` = best DAG over all pages (balanced weights).
- `low_confidence_dag` = best DAG by quality alone over all pages.

## Report

`report.md`: per class and profile — chosen DAG, utility, quality, latency, GPU-seconds,
page count, trust; comparison with the currently active list; routing coverage
(share of production-like pages that hit each rule). Promotion review uses this report.
