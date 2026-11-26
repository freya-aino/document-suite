# ds-itest — Specification

The integration suite: the deployed system, driven over its public API. Runs against
[`deploy/compose`](../../deploy/compose/README.md). System spec:
[`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: the stack lifecycle and a reachability gate (`stack_or_skip`), a typed client
over the API built on **`ds-api`'s own response types** (`login`, `upload`, `await_job`, `document`, `page`, `activate_dag_list`,
`metrics`), corpus generation and upload, a Prometheus text reader with interpolated
histogram quantiles and per-phase deltas, and the adapter that turns a served
`ds_core::PageResult` into the scorer's own `PageEval`.

**It runs.** `tests/smoke.rs` takes a generated corpus through the deployed API and
asserts the things that were previously asserted nowhere: that a token minted outside the
network is accepted inside it, that an unauthenticated read is refused, that a DAG list
uploaded through the API is accepted by the production registry, that every document
reaches `done` with no failed page, that the state ladder has no gaps, that each page's
blocks carry the step and model that made them, that both routing paths are taken, and
that another organization cannot read this one's document. `tests/fidelity.rs` scores what
the API served and emits `evals.jsonl` and `report.md` that `ds-adapt select` and
`report::render` consume **unchanged** — which is the property the whole black-box path
rests on.

**What none of this is.** On the `fake` tier the model answers come from `ds-testkit` and
are ours, so the suite proves the system is wired and says nothing about parsing quality.
The one real measurement there is `text` on born-digital pages, which comes from the PDF's
own text layer with no model in the path.

**Latency is calibrated per host.** `tests/latency.rs` probes the host, uploads a corpus
with fresh bytes (identical bytes would measure the render cache, not the renderer),
discards the warm-up documents, and compares p50s against
`deploy/compose/baselines/<host_key>.json` as ratios normalised by the host's own speed. A
host with no baseline reports and skips — a new machine never fails the suite.

Four of its rules were **corrected by its own measurements**, and each is pinned by a test:

| What was wrong | What the data said | The rule now |
|---|---|---|
| CPU probe took the median | A baseline probed while the stack was starting read 2.4x slow, and a run 30% *faster* was reported as a 69% regression | Minimum of twenty runs: a CPU benchmark's error is one-sided, so the fastest sample is the honest one |
| One warm-up document | A baseline recorded right after `stack.sh up` sat at 564 ms p50 while every later run on the same host was 293–421 ms | Two warm-up documents (`WARMUP_DOCUMENTS`) |
| Tail percentile gated | Across ten documents the maximum ranged 142 ms–6221 ms between runs while the median held inside 117–173 ms | Tail reported, gated only above `MIN_DOCUMENTS_FOR_TAIL` (60) |
| Every quantity gated and fully normalised | `GET /readyz` runs in ~1 ms, so 0.5 ms of noise was a 40% "regression"; and 15% probe drift turned an identical 147 ms median into x0.47 | `MIN_COMPARABLE_MS` floor, and a `CPU_FACTOR_DEADBAND` below which hosts count as equal |

Six consecutive runs then passed with normalised ratios of 0.75–1.11 against a 1.40
tolerance, while raw times moved 151–248 ms with machine load — which is the
normalisation doing its job rather than the gate being loose.

Remaining:
- [ ] **`host_ms` / `model_ms` split.** `Cost` carries document wall time and model-node
      wall time, which is enough to report but not to gate: the gateable quantity is the
      model-free critical path, and that needs the DAG topology alongside the traces.
      `PageResult` carries per-block provenance but no node traces, so the critical path
      is not reconstructible from the API alone today.
- [x] **Tokens per document**, from the `model_tokens_total` delta around each upload —
      the number no in-process path can report, since `RunRecord::tokens` is 0 everywhere
      because the count never reaches the executor. Plumbing `ds_core::Provenance` so an
      in-process run has it too remains undone.
- [ ] **The DAG proposal run.** `ds-adapt propose` does not exist yet, and the comparison
      against `config/dag-list.example.json` is `report::render`'s existing
      "Comparison with current list" section, which needs the incumbent's DAGs in the
      candidate set to be fair.
- [x] The host/model latency split, which used to be blocked. `ds_core::PageResult` carries
      `traces` as of schema version 2 — every node with its duration and its executor
      level — so `page.critical_path_ms()` gives `(host_ms, model_ms)` from what the API
      served, and the parse paths come from the same traces. The blocker was real: a
      block records the node that produced it, a node that produced no block left no
      record at all, and the level structure was not recoverable from blocks. A version 1
      page reports `None` rather than zero.
- [x] The response types in `src/client.rs` are **no longer mirrors**. `ds_api::{Accepted,
      JobStatus}` and `ds_vault::ops::{DocumentSummary, PageSummary}` are `pub` and
      round-trip, so this client deserialises the struct the handler serialised. `between`
      and `finished` moved onto `JobStatus` with it, and ~40 lines of hand-rolled civil-date
      arithmetic went away: the timestamps arrive as `chrono::DateTime` because that is
      what the server sent.
- [ ] Nothing here runs the `cpu` or `gpu` tier yet. Every number in this crate so far was
      produced against `fake`. `.github/workflows/fidelity.yml` is the workflow that would
      change that: `workflow_dispatch` plus a weekly schedule on a `[self-hosted, gpu]`
      runner, deliberately **not** gating anything.
- [ ] No fidelity figure is gated, and on this corpus none can be — see
      `tests/fidelity.rs`'s header for the arithmetic.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A21 | The suite scores what the API serves. A metric it cannot compute from `DocumentSummary` and `PageResult` is not measured black-box — entities, assertions and the graph are counts only, so Stage 2 fidelity needs a new read route. |
| A22 | The generated corpus measures the machinery, not document parsing. Its pages share one template per shape, so they are not independent samples and no per-class claim follows from them. |
| A23 | Latency is judged against a per-host baseline as a ratio, never against an absolute target. Nothing in this system is sized. |

## Layout

```
src/stack.rs     where the stack is; the reachability gate
src/client.rs    the typed client over the public API
src/corpus.rs    ds_adapt::dataset's corpus, and its ground truth
src/eval.rs      PageResult -> PageEval, through the scorer's own code
src/promtext.rs  Prometheus text: labels, sums, quantiles, deltas
tests/smoke.rs   end-to-end functionality
tests/fidelity.rs  scoring, and the artifacts ds-adapt consumes
```

## How it is run

```sh
deploy/compose/stack.sh up --tier fake
eval "$(deploy/compose/stack.sh env)"
cargo test -p ds-itest --test smoke --test fidelity -- --ignored --nocapture
cargo test -p ds-itest --test latency -- --ignored --nocapture   # alone: see below
deploy/compose/stack.sh down
```

**The latency test runs alone.** The service is configured for one document at a time, so
an upload from another test queues behind it and the queue time lands in the measurement.
It passes either way — the normalisation absorbs it — but a number taken under contention
is a measurement of the test runner.

**Every test uploads bytes of its own** (`corpus::freshened` with a per-test
`corpus::nonce`). The generator is deterministic, which is the right property for a corpus
and the wrong one for isolation: two tests that each generate "their own" corpus produce
the same bytes, the same content hash and therefore the same document on the server, and
then race over its current run. That is not hypothetical — it is how
`GET /v1/documents/{hk}` came to report `processing` to one test while another was still
working. It also keeps each run off the render cache, which ingest keys by content hash.

`#[ignore]` is the house convention for a test needing a service, and `stack_or_skip` sits
on top of it so `cargo test --workspace -- --include-ignored` does not fail for someone
with no Docker running. A test that is reached fails loudly.
