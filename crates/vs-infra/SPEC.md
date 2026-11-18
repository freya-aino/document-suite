# vs-infra — Specification

Thin adapters to infrastructure, behind traits so the pipeline crates never touch a
client library directly. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) — this crate
owns the §5.3 lane, §5.4 reliability and §5.5 observability contracts.

## Status

Implemented: the NATS JetStream bus in full (stream topology, DLQ, max-deliveries
advisories, lane fairness, message de-duplication, double acks, the exhausted-task give-up
path), the S3/Garage blob store, TOML + environment config, and `telemetry`: structured
logs, the one process-wide Prometheus recorder, and OTLP span export. 8 unit tests pass;
6 integration tests, 4 of which need NATS and Garage from `devenv up`.

Remaining:
- [ ] Nothing samples. Every span is exported, which is affordable at one span per task
      and per DAG node and will not stay so once the model client's per-attempt span is
      included at volume.
- [ ] Encryption at rest for originals (§5.6) is asserted but not configured here.
- [ ] No Kafka implementation behind `TaskBus` (A6 allows it; nothing needs it yet).

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A6 | Task bus is **NATS JetStream**; the `TaskBus` trait allows swapping in Kafka. |
| A7 | Object storage is S3-compatible (Garage locally and in production). |

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
`postgres`, `qdrant`, `models` (path to model registry file), `classify`, `worker`,
`stage2`, `api`, `telemetry`. Secrets only from environment. See `config/worker.example.toml`.

`stage2` (`Stage2Config`, all keys defaulted) names the models of the `page` classifier and
the `doc` role by registry id (`extraction_model`, `verify_model`, `embedding_model`,
optional `judge_model` and `classifier_model`) plus the Stage 2 thresholds
(`window_chars`, `type_match_threshold`, `link_threshold`, `judge_band`,
`embed_batch_size`) and `dag_list_refresh_s`, how long a `page` worker trusts its cached
tenant → active-DAG-list mapping (default 60).

## Telemetry (`telemetry`)

`telemetry::init(cfg, service)` is the one entry point, called once per process from
inside the Tokio runtime. It installs the `tracing` subscriber (JSON logs on
`json_logs`), installs the process-wide Prometheus recorder, and returns its handle.

A second call returns an error rather than panicking or quietly shadowing the first,
which is the only safe behaviour for something process-global.

`service` becomes a **global label on every series** — `vs-suite` for a process running
the whole service, `vs-suite-page` or `vs-suite-api-ingest` for one running part of it
(`--roles`) — so one Prometheus scraping the whole fleet can tell
whose counter it is reading. The worker's role is therefore part of the label, and `init`
is called after the role is parsed.

Where the series are *served* is the asymmetry worth knowing:

| Binary | `metrics_addr` | How `/metrics` is reached |
|---|---|---|
| without the `api` role | set | the exporter binds it and answers `GET /metrics` itself — there is no HTTP server in the process otherwise |
| with the `api` role | unset | the handle `init` returns is rendered on the API's own `/metrics` route, which requires the admin role |

Setting `metrics_addr` *and* running the `api` role binds a second port serving the same
registry — and that port has no authentication, while `/metrics` does, so bind it
internally or leave it unset;
nothing stops it, but there is no reason for it. Leaving it unset on a worker is not an
error either — the recorder is still installed, so nothing is dropped, but nothing can
scrape it. **Bind the microVM's internal interface, never `0.0.0.0`**: there is no auth
in front of a worker.

Histogram buckets are set once, globally, from 5 ms to 10 minutes. The exporter's own
defaults stop near 10 s, which would put a page render, a VLM call and a whole Stage 2
document into one overflow bucket — the three durations anyone actually wants a quantile
for.

Spans go to `otlp_endpoint` over **OTLP HTTP/protobuf**, batched, with the same
`service.name` the Prometheus series carry so a trace and a counter from one process are
joinable. The exporter is built before the subscriber, so an endpoint that cannot be
parsed is a startup failure rather than a silent fall back to logs-only.

Two things a deployment has to get right, both of them things that fail silently:

- **The port is 4318, not 4317.** HTTP/protobuf rather than gRPC because `reqwest` with
  rustls is already a dependency here and `tonic` is not, and every collector accepts
  both. `init` appends `/v1/traces` itself, so configure the bare origin.
- **Call `Telemetry::shutdown` before exiting.** The batch processor holds spans for up
  to its flush interval, so without it a process loses the last batch — which is the one
  covering whatever made it shut down. `vs-suite` calls it after the drain and
  after `serve` returns.

The SDK's batch processor owns a dedicated thread with no Tokio reactor on it, so the
exporter uses reqwest's **blocking** client, which brings its own. The async client
compiles and then panics on that thread, losing every span while the process looks
healthy. `tests/traces.rs` runs a real socket and asserts a span arrives, because that
is the only kind of test that catches it.

§5.5's standard fields are spread over the two spans that have them, not repeated on
every span — and never an identifier of a person, which `docs/SPEC.md` §5.6 states as a
rule and `vs-api` enforces with a test:
`task` (a worker role) carries `role`, `job_id`, `run_id`, `document_hk`,
`page_no` and `attempt`, with the last two empty for the roles that have no such value
yet; `dag_node` (`vs-dag`) carries `node_id`, `step`, `dag`, `page_no` and `run_id`.
`dag_node` nests inside `task`, so a page's trace is the shape of the DAG that ran it,
and a `model_call` span nests inside the node that made the call.
