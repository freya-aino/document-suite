# deploy/gpu — Specification

GPU tier: model servers reached by workers over an OpenAI-compatible HTTP API
(decision D3). Default server: **vLLM** (assumption A1). Runs on bare metal or Kubernetes
(assumption A4); if GPU workloads must be VM-isolated, use Cloud Hypervisor with VFIO
passthrough (Firecracker cannot do this). System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) — decision D1.

## Status

Implemented: `docker-compose.yml` with one vLLM service per pool **plus a reserved
interactive pool behind a profile**, `models.compose.toml` as the matching model registry,
`k8s/` with a Deployment, Service and HPA per pool **including the interactive lane**, and
an optional Prometheus profile. See "What exists here" below for the detail.

**The interactive lane exists** (decision D15). The service reserving page slots for
interactive uploads was only half the answer: it stops a batch from filling a *copy* and
does nothing about the batch's requests already queued in a vLLM instance, where a
thousand batch pages and one interactive page queue together. `doc-parse-interactive`
serves the same weights under the same served name on its own replicas, the registry's
`interactive_endpoints` points at it and nothing else does, and it is sized for latency
rather than throughput — fewer concurrent sequences, a floor of one replica so it is never
cold, and a queue-depth target a third of the shared pool's. A deployment with one pool
declares nothing and is unaffected.

**A local path exists too**: `config/models.litellm.toml` plus `config/litellm.yaml` run
the whole model tier behind one LiteLLM proxy on `:4000` (`vs-litellm-up`), so Stage 1 can
be exercised without a GPU at all.

**None of it has been run against real GPUs.** The vLLM flags, memory fractions and HPA
thresholds are reasoned starting points, not measured ones.

Remaining:
- [ ] Validate every pool actually serves on real hardware, and correct the vLLM settings
      below from measurements rather than estimates. This is the whole of what is left
      here, and no amount of code closes it.
- [ ] Decide GPU hardware and the expected pages/day range — the open question that blocks
      pool sizing, tensor-parallel degrees and the batch-vs-interactive split. The
      interactive lane is *implemented*; how many replicas it deserves is a measurement.
- [ ] The HPA keys on the Pods metric `vllm-num-requests-waiting` through a
      `kube-metrics-adapter` annotation; it needs porting to whichever adapter the target
      cluster runs (prometheus-adapter or KEDA).

### No sparse-embedding pool: a decision

Lexical search is Qdrant sparse vectors computed **in process**, in Rust: tokenise, emit
term frequencies, let the collection's `modifier: idf` supply the rest of BM25 server-side.
That needs no pool here and no Python in a Rust pipeline (goal G9), and both halves are
now queried and fused inside Qdrant — see
[`vs-vectors`](../../crates/vs-vectors/SPEC.md).

If BM42-quality sparse embeddings are wanted later, the upgrade is a pool here serving a
sparse encoder and a `vs-models` embedder in front of it; the `VectorStore` interface does
not change, only where the sparse vector comes from. Nothing about the current design
forecloses it, which is why there is no placeholder pool for it.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A1 | "VLLM" means vision-language models served by **vLLM**; the OpenAI-compatible client works with other servers too. |
| A3 | NVIDIA GPUs; exact type unknown. Model pools are configured per model. |
| A4 | Model servers run outside Firecracker (bare metal / Kubernetes). |

## Pools

One pool per model (a pool = N replicas behind the endpoints listed in
`config/models.example.toml`). Typical v1 set:

| Pool | Model class | Used by steps |
|------|-------------|---------------|
| `doc-parse` | compact document VLMs (e.g. PaddleOCR-VL-1.5, MinerU2.5, dots.ocr) | `page.parse_e2e`, `region.ocr`, `table.recognize`, `formula.recognize` |
| `layout` | layout detector exposed via the same API, or a doc VLM in layout mode | `layout.detect` |
| `vlm-general` | general VLM (e.g. Qwen3-VL family) | `figure.describe`, `chart.to_table`, judge in vs-adapt |
| `classifier` | small VLM | tier 1 page classification |
| `llm-extract` | text LLM with strong structured output | Stage 2 extraction and verification |
| `embed` | text embedding model (`/v1/embeddings`) | Stage 2 embeddings, type normalisation, entity resolution |
| `doc-parse-interactive` | the same weights as `doc-parse`, reserved | every Stage 1 step, for interactive uploads only |

Which document VLMs end up in production is decided by `vs-adapt` scores, not here.
Keep 2–3 candidates per role served in stage.

The reserved pool is the only one whose *existence* is a policy decision rather than a
capability: it costs a GPU to keep latency off the batch's critical path. A deployment
that does only batch work leaves it out of `kustomization.yaml` (or does not pass
`--profile interactive`) and declares no `interactive_endpoints`, and nothing else
changes.

## vLLM settings (per pool, starting points)

```sh
vllm serve <model> \
  --served-model-name <id> \
  --max-num-seqs 256 \            # high concurrency; workers keep many requests in flight
  --enable-prefix-caching \       # shared prompt prefixes across pages
  --limit-mm-per-prompt '{"image": 1}' \
  --gpu-memory-utilization 0.92
```

- Structured output (guided decoding) must be enabled for pools used with JSON schemas.
- Pin weights by revision; the registry `revision` must match what is served.
- Expose Prometheus metrics; the autoscaler uses `num_requests_waiting` and
  GPU utilisation per pool.

## What exists here

- `docker-compose.yml`: one vLLM service per pool (`doc-parse`, `layout`, `vlm-general`,
  `classifier`, `llm-extract`, `embed`) with the flags above, `--served-model-name` equal
  to the registry id, `HF_TOKEN` from the environment, per-service GPU reservations, a
  shared weight cache and an optional Prometheus (`--profile monitoring`, `prometheus.yml`).
- `models.compose.toml`: the registry for that compose deployment (ports 8001–8006).
- `k8s/`: a Deployment + Service per pool and an `autoscaling/v2` HPA on the Pods metric
  `vllm-num-requests-waiting` (PromQL in a `metric-config.…prometheus/query` annotation
  for kube-metrics-adapter; adaptable to prometheus-adapter or KEDA), `kustomization.yaml`.
- `README.md`: how `config/models.toml` endpoints map to these services.

## Scaling

- Scale each pool on queue depth (`num_requests_waiting`) and p95 latency.
- `page` role autoscaling is capped by GPU pool latency (see
  `deploy/firecracker/SPEC.md`), so CPU workers never flood the GPUs.
- Batch traffic can use cheaper or preemptible GPU nodes. **The interactive lane is a
  reserved replica set**, not a header on a shared one: the request is routed by the
  client, from the upload's own priority, through `interactive_endpoints` in the model
  registry (D15). Routing by header would still have put the request in the same vLLM
  queue, which is the problem.
- The reserved pool's HPA has a floor of one replica and a queue-depth target a third of
  the shared pool's: someone is waiting on every request in it, so it should scale out
  sooner and never scale to zero.

## Throughput levers (in order of impact)

1. Routing that skips GPU work on born-digital pages (vs-router / vs-adapt).
2. Compact document VLMs (≈1B parameters) for Stage 1 instead of large general VLMs.
3. High in-flight concurrency + continuous batching.
4. Render DPI no higher than the model's native input size needs.
5. Prefix caching for fixed prompts.
