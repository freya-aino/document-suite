# vs-infra — Specification

Thin adapters to infrastructure, behind traits so the pipeline crates never touch a
client library directly.

## Task bus (`queue`)

Trait `TaskBus`: `publish(lane, task)`, `subscribe(role, lane) -> stream of Delivery`,
`Delivery::{ack, nak(delay), term}`.

Default implementation: **NATS JetStream** (assumption A6).

| Stream | Subjects | Retention | Consumer (durable, pull) |
|--------|----------|-----------|--------------------------|
| `VS_TASKS` | `vs.<role>.<lane>` — role ∈ `ingest, page, doc`; lane ∈ `interactive, batch` | work-queue | `w-<role>-<lane>` |
| `VS_DLQ` | `vs.dlq.<role>` | limits (14 days) | inspected by ops |
| `VS_ADVISORIES` | `$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.VS_TASKS.*` | work-queue (14 days) | `adv-<role>` (filters its role's two consumers) |

- Messages are JSON `vs_core::Task` (references only; ≤ 16 KiB).
- `ack_wait` per role: ingest 10 min, page 5 min, doc 30 min. Long tasks send
  `in_progress` heartbeats every `ack_wait / 3`.
- `max_deliver = 4` (1 + 3 retries). Retry delays `[10s, 60s, 300s]` are applied by the
  worker's `nak`; the consumers have **no `backoff`**: with a backoff list JetStream
  uses it instead of `ack_wait` (a 10 s ack window on the first delivery, so long tasks
  were redelivered while still running) and ignores nak delays. A crashed worker's
  task is redelivered after `ack_wait`.
- On a handler error at the last attempt (or a permanent error) the worker publishes
  to `VS_DLQ` and terminates the message.
- If every attempt ends without a result (worker crash or hang each time), no handler
  runs its give-up path. JetStream then emits a max-deliveries advisory, which
  `VS_ADVISORIES` stores durably; `subscribe_exhausted(role)` yields the stuck task,
  the role runs its give-up path, dead-letters it and deletes it from `VS_TASKS`. The
  advisory is emitted on the next delivery attempt on that consumer, i.e. while some
  worker of the role is pulling.
- Message id header = `<job_id>:<role>:<page_no?>` → JetStream de-duplication window
  (2 min) prevents duplicate fan-out on API/ingest retries.
- Pulls fetch `pull_batch` messages per request (default 1): fetched messages count
  against `ack_wait` while they wait in the client, and workers pull only when a slot
  is free. Acks are double acks (server-confirmed).
- Undecodable messages are terminated immediately (no retries).
- `ensure_topology()` creates or updates both streams and all six consumers; every
  worker and the API call it at startup.
- Lane fairness: each worker pulls from `interactive` first, with at most
  `interactive_share` (default 0.3) of its in-flight slots reserved for it.
- Autoscaling reads consumer `num_pending` per role and lane.

A Kafka implementation can be added behind the same trait (partition by `document_hk`).

## Blob store (`blob`)

Trait `BlobStore`: `put(key, bytes, content_type)`, `get(key)`, `exists(key)`,
`presign_get(key, ttl)`. Implementation via the `object_store` crate:
`ObjectBlobStore::s3(&BlobConfig)` (credentials from `AWS_*` env, path-style requests,
plain HTTP allowed when the endpoint is `http://`; Garage locally and in production)
or `ObjectBlobStore::new(store)` for tests (no presigning).

Key layout (content-addressed where possible):

```
originals/<tenant>/<sha256>                         original upload
pages/<document_hk>/<page_no>/<dpi>.png             rendered page
pages/<document_hk>/<page_no>/thumb.jpg             classifier thumbnail
pages/<document_hk>/<page_no>/signals.json          classifier tier-0 signals (written last)
textlayer/<document_hk>/<page_no>.json              native text spans
crops/<page_hk>/<run_id>/<block_index>.png          figure/chart crops
results/<document_hk>/<run_id>/page-<page_no>.json  PageResult
results/<document_hk>/<run_id>/doc.json             Stage 2 output (graph + assertions)
```

Rendered pages are reused across runs at the same DPI (no re-render on reprocessing).

## Config (`config`)

TOML file + environment overrides (`VS__SECTION__KEY`). Sections: `bus`, `blob`,
`postgres`, `helix`, `models` (path to model registry file), `classify`, `worker`,
`stage2`, `api`, `telemetry`. Secrets only from environment. See `config/worker.example.toml`.

`stage2` (`Stage2Config`, all keys defaulted) names the models of the `page` classifier and
the `doc` role by registry id (`extraction_model`, `verify_model`, `embedding_model`,
optional `judge_model` and `classifier_model`) plus the Stage 2 thresholds
(`window_chars`, `type_match_threshold`, `link_threshold`, `judge_band`,
`embed_batch_size`) and `dag_list_refresh_s`, how long a `page` worker trusts its cached
tenant → active-DAG-list mapping (default 60).

## Telemetry (`telemetry`)

`tracing` with JSON logs; OTLP exporter when `telemetry.otlp_endpoint` is set; Prometheus
endpoint on `telemetry.metrics_addr` (workers expose it on the VM's internal interface
only). Standard span fields: `job_id`, `run_id`, `document_hk`, `page_no`, `node_id`.
