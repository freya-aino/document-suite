# ds-adapt — Specification

Runs in **dev/stage only**. Given datasets of documents, it generates candidate Stage 1
DAGs, runs them, scores them per page class, and emits a **DAG list** (DAGs + routing
table) to promote to production. This is where the "best performing workflow per
language/content" knowledge is produced (G3). System spec:
[`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: all five commands (`generate`, `run`, `score`, `select`, `promote`), **two
more** (`dataset`, `classify-score`), template × slot candidate generation validated
against `ds_steps::production_registry`, successive halving with resumable `runs.jsonl`,
the page metrics (edit-distance text similarity, TEDS for tables, LaTeX similarity,
reading order, layout F1 at IoU 0.5), the judge path for unlabelled data, the objective
with per-class back-off, and `report.md`. 43 unit tests pass.

**The chain runs.** `tests/chain.rs` takes a generated corpus through
`dataset → prepare → run → score → select → report` against an in-process fake model tier
on a real socket, and asserts the selected DAG list loads in the production router — the
same check the API's upload route makes. Before this, the five commands had never run as a
chain, and the reason was that there was nothing to run them on.

**There is a corpus.** `ds-adapt dataset` writes small PDFs of four page shapes — prose,
two-column, form, ruled table — with the ground truth emitted from the same geometry the
pages were written with, so the labels are mechanically exact rather than annotated.

**The classifier is scored.** `ds-adapt classify-score` reports per-class precision and
recall, a directional confusion table (a paper read as a form takes a cheap DAG and loses
its tables; the reverse merely wastes GPU) and a calibration curve, with the classifier run
*without* the manifest's hint — the hint is the label. Its first run found a real defect in
`ds-classify`: a rules-only page claimed 0.85 confidence for a document type nothing had
determined, and so sailed past the threshold meant to catch it.

**What none of this is.** The corpus is clean, born-digital, one typeface, no skew, no
handwriting, no photographs, and the chain test's model tier answers the same three lines
to every page. A DAG that wins there has won on that, which is a statement about the
machinery and not about document parsing.

Remaining:
- [ ] **Run it on a real labelled corpus and check the selected DAGs beat the hand-written
      `config/dag-list.example.json`.** Until that happens, "content-aware routing" (G2) is
      an untested claim. This is the one item here that no amount of code closes: it needs
      documents, labels and a model tier.
- [ ] The conditioning and cross-check templates in `search-space.example.json` are
      validated and have never been scored on real pages, so whether deskewing or
      binarising helps any page class — or whether a second parser's corroboration
      (`e2e-cross-checked`) is worth its doubled per-page cost — is still the open
      question they exist to answer.
- [ ] Stage 2 is not in the search (A17), so nothing measures whether a different
      extraction shape would do better. That needs a second objective — facts per document
      against cost — rather than a second search space.
- [ ] `promote` uploads as a candidate; activation stays a manual API call by design.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A13 | DAG scoring = quality floor per page class, then weighted quality vs latency vs GPU cost. |
| A15 | Datasets may lack labels; then scoring uses agreement between strong models / a judge model, marked as lower-trust scores. |
| A16 | `ds-adapt` searches over a registered step catalogue and its parameters (model, prompt template, DPI, crop strategy), not free-form prompts. |

```
train → DAG list → prod upload → classification → selected DAG
```

## Commands

| Command | Input | Output |
|---------|-------|--------|
| `ds-adapt generate` | `search_space.json`, step catalogue (`ds_steps::production_registry` built from `--models`) | `candidates.json` (validated DAGs) |
| `ds-adapt run` | dataset manifest, candidates, `--models`, render child | `out/pages/<doc>/<page>.png`, `out/results/<doc>/<page>/<dag>.json` (page result per (page, DAG)) + `out/runs.jsonl` (latency, GPU-seconds, tokens) |
| `ds-adapt score` | `runs.jsonl`, ground truth (optional), `--judge-model` (optional) | `evals.jsonl` (`PageEval` per page × DAG) |
| `ds-adapt select` | evals, objective, candidates, `--current` (optional) | `dag-list.json` + `report.md` |
| `ds-adapt promote` | dag-list.json, `--api`, `--token` (`DS_API_TOKEN`) | uploads as `candidate` via `POST /v1/dag-lists` as a bearer token holding the admin role; the organization is the token's, not a flag (activation stays a manual step) |
| `ds-adapt dataset` | `--out`, shape counts | `docs/*.pdf`, `ground-truth/*.json`, `manifest.jsonl` — a corpus labelled from the geometry it was written with |
| `ds-adapt classify-score` | manifest, `--models`, optional `--classifier-model` | the page classifier's accuracy, confusions and calibration, as Markdown |

```
ds-adapt generate --search-space config/search-space.example.json --out candidates.json [--models config/models.example.toml]
ds-adapt run --manifest data.jsonl --candidates candidates.json --out runs/ [--models …] [--sample 4] [--rounds 3]
             [--max-pages 50] [--render-child target/debug/ds-render-child] [--classifier-model page-classifier]
             [--blob-config blob.toml] [--split train] [--dpi 200] [--parallel 4] [--render-pool N] [--objective o.json] [--weights w.json]
ds-adapt score --runs runs/runs.jsonl --out evals.jsonl [--weights w.json] [--judge-model qwen3-vl] [--models …]
ds-adapt select --evals evals.jsonl --candidates candidates.json --version 2026-11-06 --out dag-list.json
                [--objective o.json] [--current active.json] [--report report.md]
ds-adapt promote --dag-list dag-list.json --api http://localhost:8080 --tenant acme
ds-adapt dataset --out corpus/ [--docs-per-shape 3] [--pages-per-doc 2] [--language en]
ds-adapt classify-score --manifest corpus/manifest.jsonl --out classify/ [--models …]
                        [--classifier-model page-classifier] [--split holdout] [--report classifier.md]
```

The whole chain, from nothing:

```sh
ds-adapt dataset --out corpus/
ds-adapt generate --search-space config/search-space.example.json --out candidates.json
ds-adapt run --manifest corpus/manifest.jsonl --candidates candidates.json --out runs/ --models models.toml
ds-adapt score --runs runs/runs.jsonl --out evals.jsonl
ds-adapt select --evals evals.jsonl --candidates candidates.json --version $(date +%Y-%m-%d) \
                --out dag-list.json --report report.md
ds-adapt classify-score --manifest corpus/manifest.jsonl --out classify/ --report classifier.md
```

`tests/chain.rs` is that sequence as a test, against a fake model tier.

`run` is a plain process on the stage cluster: it renders through a `ds_ingest::RenderPool`
of `ds-render-child` processes (default: the binary next to `ds-adapt`), classifies with the
production classifier (tier 0 rules; tier 1 only with `--classifier-model`), and executes
every candidate with `ds_dag::Executor` against the model endpoints in `--models`. Local
paths are read directly; `s3://` URIs need `--blob-config` (TOML: `bucket`, `endpoint`,
`region`; credentials from the environment). `runs.jsonl` is appended to, so a run can be
resumed by re-running with the same `--out`.

## Datasets

Manifest (JSONL), one line per document:
`{ uri, language, doc_type_hint?, ground_truth_uri?, split: "train"|"holdout" }` (`split`
defaults to `train`; `--split` filters). Public sets to seed: OmniDocBench, PureDocBench,
ParseBench, Dr. DocBench, plus your own documents.

**`ds-adapt dataset` writes one**, which is what makes the chain runnable with nothing to
hand. Four page shapes, each a routing class worth telling apart — prose, two columns with
a real gutter, a labelled field grid, a ruled table with a caption — as hand-written PDFs,
with the ground truth derived from the same line positions and type sizes that placed the
text. The one approximation is a line's width, from Helvetica's average advance: close
enough to match at IoU 0.5 (what `layout_f1` uses) and far too crude to score a layout
model at IoU 0.9, so nothing claims to.

It is synthetic and says so: born-digital, clean, one typeface, no skew, no handwriting,
no photographs, no entities to extract. It exercises the machinery and calibrates nothing.

Ground truth (`ground_truth_uri`, local path): a JSON file with `{"pages": [page, …]}`, a
bare array of pages, or one page object (single-page documents); or a directory with
`<page>.json` / `page-<page>.json` (1-based). A page is OmniDocBench-like:

```jsonc
{ "width_px": 1654, "height_px": 2339,           // only when bboxes are in pixels
  "blocks": [                                    // alias: layout_dets
    { "kind": "paragraph",                       // aliases: category, category_type, label;
                                                 // our snake_case kinds, parser labels or
                                                 // OmniDocBench categories (text_block, equation_isolated, …)
      "bbox": [x0, y0, x1, y1],                  // normalised or pixels; {x0,y0,x1,y1}; or "poly": [x,y,…]
      "text": "…", "html": "<table>…", "latex": "…",
      "order": 0 } ] }                           // reading order; file order if absent
```

All pages are classified with the production classifier first, so scores are grouped by
the **same `PageProfile` the router will see** in production (classifier errors are part
of the measurement, not hidden). `doc_type_hint` reaches the classifier as a caller's hint
would — except under `classify-score`, which withholds it, because there the hint is the
label and a classifier handed its own label would score perfectly while saying nothing.
A `doc_type` in a ground-truth page overrides the manifest's hint, for a corpus whose
documents are not each one type.

## Candidate generation

`search_space.json` lists **templates** — DAG skeletons with parameter slots — and the
values each slot can take:

```jsonc
{
  "templates": [
    { "id": "e2e", "dag": { "output": "parse", "nodes": [ /* "{model}" slots */ ] },
      "slots": { "model": ["paddleocr-vl-1.5", "mineru-2.5", "dots-ocr"] } },
    { "id": "native+layout+tables", "dag": { … },
      "slots": { "layout_model": ["pp-doclayout-v3"], "table_model": ["paddleocr-vl-1.5", "mineru-2.5"] } }
  ],
  "max_candidates": 200
}
```

- `{slot}` is substituted in **every string** of the skeleton JSON (params, ids, prompts).
  A string that is exactly one placeholder takes the slot's JSON value, so numeric slots
  (DPI, thresholds) stay numbers; otherwise the value's text is spliced in.
- Cartesian product per template (slots in name order), capped by `max_candidates` over
  all templates in file order; every candidate must pass `Dag::validate` against the
  production step registry. Rejected candidates are printed, not written.
- Candidate id = `<template>/<slot=value,…>` (just `<template>` without slots);
  version = first 16 hex chars of SHA-256 over the canonical (key-sorted) JSON of
  `output` + `nodes`, so equal content gets an equal version.

Templates encode the "skip" structure (e.g. text-layer DAGs without OCR), so the search
explores cheap DAGs as well as robust ones.

To control cost, `run` uses **successive halving**: round *r* runs every surviving
candidate on the next `sample · 2^r` unused pages of each class (class = fully discretised
profile), ranks candidates by a provisional utility over everything run so far and keeps
the top third (`ceil`, at least one). After `--rounds` rounds (or once one candidate is
left) the survivors run on all remaining pages. Provisional quality = labelled quality
when the page has ground truth, otherwise a judge-free proxy (block count saturating at
20, 0 for failures); the utility is the balanced objective **without** the quality floor.

### Run records

`runs.jsonl`, one line per (page, DAG):
`{ doc, page, dag_id, dag_version, class_key, profile_json, latency_ms, gpu_seconds, tokens,
ok, error, result_path, image_path, ground_truth_uri, split, blocks }`. Paths are relative
to `--out`. `gpu_seconds` is the wall time of nodes that reported a `model_ref` (the
executor exposes no token counts yet, so `tokens` is 0); `class_key` is the JSON of the
level-0 `Match`. The page result (`results/<doc>/<page>/<dag>.json`) holds the block drafts
plus a per-node trace (step, model, prompt, duration, outcome).

## Scoring

Per page × DAG (`PageEval`):

| Metric | Measure |
|--------|---------|
| text | 1 − normalised Levenshtein distance on reading-order text (text-like blocks only; whitespace collapsed) |
| tables | TEDS: for every ground-truth table, the overlapping prediction with the highest IoU, scored `1 − TED / max(|T_pred|, |T_gt|)`; mean over tables; a table without prediction scores 0 |
| formulas | normalised LaTeX edit distance (whitespace removed), same per-block matching; CDM needs a renderer and is left out |
| reading order | predicted blocks matched to ground truth by IoU ≥ 0.5; 1 − edit distance between the matched order and the ground-truth order, normalised by the number of ground-truth blocks (missing blocks count) |
| layout | micro F1 at IoU 0.5, one-to-one matching within kind families (title/heading, text-like, table, figure/chart, formula, caption, header/footer/page number, stamp) |
| **quality** | weighted mean of the applicable metrics (`MetricWeights`, defaults text 1, tables 1, formulas 1, reading order 0.5, layout 0.5; metrics that do not apply to the page are left out) |
| latency_ms | wall time of the DAG for this page |
| gpu_seconds | wall time of model nodes (token-based estimate once the client exposes usage) |
| trust | `labelled` (ground truth) or `judge` (no labels: agreement with the reference DAG plus a judge VLM rating, assumption A15) |

**TEDS implementation.** The HTML is parsed into a simplified tree (`table` → `thead`/
`tbody`/`tfoot` → `tr` → `td`; `th` = `td`; other tags dropped, their text merged into
the cell; missing wrappers synthesised; unclosed cells closed by the next structural tag).
Zhang–Shasha ordered tree edit distance with unit insert/delete; relabel costs 1 when the
tag or `colspan`/`rowspan` differ and the normalised text edit distance between two cells
otherwise. This is TEDS as defined by Zhong et al. over a slightly simplified tree.

**Judge trust.** For a page without ground truth the *reference* DAG is the one with the
highest mean labelled quality in the same run (if any page is labelled), else the medoid
by text similarity among the candidates that succeeded on that page. `agreement` = text
similarity to the reference's output (the reference itself scores 1, so judge ratings are
recommended whenever there are no labels). With `--judge-model` the page image and the
extracted Markdown are rated 0–1 by the VLM and `quality = (1 − w_judge) · agreement +
w_judge · judge` (`w_judge` = `MetricWeights::judge`, default 0.5). Failed runs score 0
and carry `failed: true`.

`PageEval` carries the individual metrics (`metrics.{text,tables,formulas,reading_order,
layout,judge,agreement}`), `dag_version`, `doc`, `page_no`, `class_key`, `tokens`,
`failed`; older files without these fields still load.

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

`report.md` is written next to the DAG list (`--report` overrides) and holds:

- **Summary**: pages evaluated (labelled / judge-only), candidates, list size, defaults,
  objective parameters.
- **Rules** per caller profile: rule id, class, chosen DAG, utility (`score`), mean
  quality, latency, GPU-seconds, page count, trust (`labelled` / `judge-only` /
  `mixed (n% labelled)`) and **coverage** — the share of evaluated pages the router sends
  to the rule (the report reproduces the router's most-specific-match selection on the
  evaluated profiles), plus the default and low-confidence rows and the routed outcome
  (mean quality/latency/GPU of the DAG each page would get). Judge-only rules are flagged.
- **Classes**: every fully discretised class with pages, trust, best balanced DAG and
  the DAG the list routes to.
- **Candidates**: per DAG — pages, failures, quality, latency, GPU-seconds, trust,
  whether it is in the list.
- **Parse paths** per DAG: which parser produced the blocks, from the node traces. A DAG
  whose pages read `markdown` had its model's output split as prose because no family
  parser recognised it — the blocks have no bounding boxes worth the name, so the layout
  metrics for those pages are near-meaningless, and the report says so rather than leaving
  a reader to wonder why a candidate scored badly. This is the question `family = "auto"`
  exists to answer, and it was unanswerable until `StepOutput::metrics` existed.
- **Comparison** with `--current`: list sizes, share of pages whose DAG changes per
  profile, and routed quality/latency/GPU of both lists over the evaluated pages.

Promotion review uses this report.

`classify-score` writes its own section — accuracy, per-class precision and recall,
calibration bands and confusions — which is about the classifier rather than the DAGs and
is therefore a separate command and a separate document.
