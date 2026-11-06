# deploy/gpu — Specification

GPU tier: model servers reached by workers over an OpenAI-compatible HTTP API
(decision D3). Default server: **vLLM** (assumption A1). Runs on bare metal or Kubernetes
(assumption A4); if GPU workloads must be VM-isolated, use Cloud Hypervisor with VFIO
passthrough (Firecracker cannot do this).

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

Which document VLMs end up in production is decided by `vs-adapt` scores, not here.
Keep 2–3 candidates per role served in stage.

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
- `vs-worker` page autoscaling is capped by GPU pool latency (see
  `deploy/firecracker/SPEC.md`), so CPU workers never flood the GPUs.
- Batch lane traffic can use cheaper/preemptible GPU nodes; interactive lane gets a
  reserved replica set (route by a header to a separate endpoint list if needed).

## Throughput levers (in order of impact)

1. Routing that skips GPU work on born-digital pages (vs-router / vs-adapt).
2. Compact document VLMs (≈1B parameters) for Stage 1 instead of large general VLMs.
3. High in-flight concurrency + continuous batching.
4. Render DPI no higher than the model's native input size needs.
5. Prefix caching for fixed prompts.
