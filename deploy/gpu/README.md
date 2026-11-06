# deploy/gpu — running the model pools

The GPU tier is a set of **vLLM** servers, one per pool, each an OpenAI-compatible
endpoint. Workers never know about pools: `config/models.toml` lists, per model id, the
endpoints that serve it, and `vs-models` round-robins over them. Design: [SPEC.md](SPEC.md).

| Pool (service / Deployment) | `--served-model-name` = registry `id` | Registry `task`s | Weights (starting point) |
|-----------------------------|----------------------------------------|------------------|--------------------------|
| `doc-parse`   | `paddleocr-vl-1.5` | page_parse, region_ocr, table, formula | `PaddlePaddle/PaddleOCR-VL-1.5` |
| `layout`      | `mineru-2.5`       | page_parse, layout, table, formula     | `opendatalab/MinerU2.5-2509-1.2B` |
| `vlm-general` | `qwen3-vl`         | figure_describe, chart_to_table, judge | `Qwen/Qwen3-VL-8B-Instruct` |
| `classifier`  | `page-classifier`  | page_classify                          | `Qwen/Qwen3-VL-2B-Instruct` |
| `llm-extract` | `extract-llm`      | extract, verify, type_normalise, entity_judge | `Qwen/Qwen3-32B` (TP 2) |
| `embed`       | `embed-text`       | embed (`/v1/embeddings`)               | `Qwen/Qwen3-Embedding-0.6B` |

The weights are placeholders for the candidates `vs-adapt` evaluates; swap the Hugging
Face id and pin `--revision` / `MODEL_REVISION`. **The registry's `revision` must match
what is served** (it is recorded in provenance). Changing the `embed` model changes the
vector space: vs-helix needs a new vector index, back-filled, before switching.

## How `config/models.toml` maps to the services

Each `[[model]]` entry's `endpoints` are the base URLs of the pool that serves that id:

| Deployment | `endpoints` in `config/models.toml` |
|------------|-------------------------------------|
| docker compose, worker on the same host | `http://localhost:800N/v1` (ports 8001–8006 in service order; ready-made: `models.compose.toml`) |
| docker compose, worker elsewhere | `http://<gpu-host>:800N/v1`, or put the services behind a reverse proxy per pool |
| Kubernetes | `http://<pool>.vs-gpu.svc:8000/v1` (the Service name is the pool name) |
| `config/models.example.toml` | DNS names `doc-parse-a.gpu.internal` etc.: one A record per replica, or one name per pool in front of a load balancer |

A pool with several replicas is either several endpoints in the list (client-side
round-robin, no balancer) or one endpoint in front of a Service / load balancer. The
`max_concurrency` of the entry bounds the in-flight requests per worker process, not per
replica; size it to `--max-num-seqs × replicas / workers`.

## docker compose (single host)

```sh
cd deploy/gpu
export HF_TOKEN=hf_...                       # gated weights
docker compose up -d                         # all pools; first start downloads weights
docker compose --profile monitoring up -d    # + Prometheus on :9090
curl -s localhost:8005/v1/models | jq .      # served ids
```

GPU placement is `deploy.resources.reservations.devices[].device_ids` per service;
`classifier` and `embed` share GPU 3 (`--gpu-memory-utilization` 0.45 / 0.40),
`llm-extract` takes two GPUs with tensor parallelism. Needs the NVIDIA container toolkit.
`prometheus.yml` labels every target with `pool` and `served_model`, so the autoscaler's
queries (`vllm:num_requests_waiting`, `vllm:e2e_request_latency_seconds_bucket`) group
by pool.

## Kubernetes (`k8s/`)

```sh
kubectl apply -k deploy/gpu/k8s
kubectl -n vs-gpu create secret generic hf-token --from-literal=HF_TOKEN=hf_... --dry-run=client -o yaml | kubectl apply -f -
kubectl -n vs-gpu get pods -l app.kubernetes.io/name=vllm
```

Per pool: a `Deployment` (one container, `nvidia.com/gpu` limit, shared weight-cache
PVC, `/dev/shm` for NCCL, startup probe long enough for download + graph compile, rolling
update with `maxUnavailable: 0`), a `Service` on 8000 and an `HorizontalPodAutoscaler`
(autoscaling/v2) on the **Pods** metric `vllm-num-requests-waiting`, target 8 queued
requests per replica. The metric reaches the HPA through the custom-metrics API: the
manifests carry a `metric-config.pods.…prometheus/query` annotation as read by
[kube-metrics-adapter](https://github.com/zalando-incubator/kube-metrics-adapter); with
prometheus-adapter write the equivalent rule (`vllm:num_requests_waiting` → `vllm_num_requests_waiting`
external/pods metric), or with KEDA a `ScaledObject` with the prometheus scaler and the same
query. Scale-up is one replica per two minutes (cold starts are slow); scale-down waits
ten minutes. Pods carry `prometheus.io/scrape` annotations for the cluster's Prometheus.

Separate replica sets for the interactive lane (SPEC.md "Scaling") are a second
Deployment + Service per pool with a different endpoint list in the registry.
