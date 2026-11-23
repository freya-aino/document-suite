# vs-infra — Specification

Thin adapters to infrastructure, behind traits so the pipeline crates never touch a
client library directly. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) — this crate
owns the §5.5 observability contract.

It used to own the task bus as well, and with it §5.3 and §5.4. Decision D13 removed the
bus: the stages are function calls in one process, so there is no delivery to make
at-least-once and no lane to be fair about. What is left here is the blob store, the
config loader and telemetry.

## Status

Implemented: the S3/Garage blob store with **server-side encryption** and **object
retention**, TOML + environment config, and `telemetry`: structured logs, the one
process-wide Prometheus recorder, OTLP span export and **parent-based trace sampling**.
8 unit tests pass; 5 integration tests, two of which need Garage from
`devenv --profile stack up`.

**Encryption at rest** (§5.6) is configured here now rather than asserted: `blob.sse` is
`none` (the default), `aes256`, `aws:kms`, `aws:kms:dsse` or `sse-c`, and a mode whose key
is missing is a startup error rather than a silent fall back to plaintext — a deployment
that asked for KMS and got nothing would believe it was encrypted. The default is `none`
because a bucket policy is the better place for it where one exists, and because sending
an encryption header a non-AWS implementation rejects would break the Garage and MinIO
deployments that are already encrypting at the volume.

**Retention** answers open question 4. `[blob.retention]` keeps each class of object for a
number of days — page images 30, results 90, text layers 30, crops 7, the scratch images
handed to models 1 — and `sweep_retention` prunes what is past it, every
`sweep_interval_h` from every copy (deleting what is older than a cutoff is idempotent, so
a race between two copies costs a wasted LIST). **Originals default to forever**: they are
the tenant's input, and deleting it is not this software's decision to make.

**Sampling**: `telemetry.sample_ratio`, parent-based, so a trace is never exported in
pieces. 1.0 (the default) is `AlwaysOn` rather than a ratio sampler at probability 1, so
the common case carries no sampling decision at all; 0.0 is an off switch and is tested as
one. A ratio outside 0.0..=1.0 is clamped and logged rather than refused, because a typo
in a telemetry setting should not stop the service.

Remaining:
- [ ] Neither exporter has run against a real Prometheus or a real collector. Both are
      tested against sockets that answer — a listener that records what it was sent, and a
      collector that asserts bytes arrive — which is what catches a setting that is read,
      logged about and ignored. What it cannot catch is a dashboard that asks for a series
      by the wrong name, or a sampling ratio that turns out to be too low to debug with.
- [ ] The blob store is the only adapter with no second implementation, which is fine
      while it is S3-compatible everywhere. Nothing here is pluggable for the sake of it:
      the `TaskBus` trait and its NATS implementation were deleted with the bus rather
      than kept for a Kafka nobody asked for.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A7 | Object storage is S3-compatible (Garage locally and in production). |

## Blob store (`blob`)

Trait `BlobStore`: `put(key, bytes, content_type)`, `put_stream`, `get(key)`,
`get_stream`, `exists(key)`, `size(key)`, `presign_get(key, ttl)`, `delete(key)` and
`prune(prefix, cutoff)`. Deleting something that is not there is not an error: every
caller here is making the store match a desired state, not asserting its history.
`prune` takes a **cutoff** rather than a duration so the clock stays with the sweep that
decides what "old" means, which is also what makes it testable without sleeping.
Implementation via the `object_store` crate:
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

Rendered pages are reused across runs at the same DPI (no re-render on reprocessing), and
a page re-rendered at a *finer* DPI for `crop_dpi` is stored under that DPI, so the next
step, the next page of the same document and the next run all find it rather than
rendering it again.

Each prefix has its own retention (`[blob.retention]`), and they differ because what they
cost to lose differs: a page image is rebuilt by re-running the document, a result is the
raw model output behind facts the vault already holds with their provenance, and an
**original** is the tenant's input and is kept forever unless a deployment says
otherwise.

## Config (`config`)

TOML file + environment overrides (`VS__SECTION__KEY`). Sections: `blob` (with
`blob.retention`), `postgres`, `qdrant`, `models` (path to the model registry file),
`classify`, `worker`, `stage2`, `api`, `auth`, `telemetry`. There is no `bus` section any
more — the bus is gone (D13). Secrets only from the environment. See
`config/worker.example.toml`.

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

`service` becomes a **global label on every series** — `vs-suite` for the service — so one
Prometheus scraping the whole fleet can tell whose counter it is reading. It is also the
traces' `service.name`, so a counter and a span from the same process are joinable.

Where the series are *served* is the asymmetry worth knowing:

| `metrics_addr` | How `/metrics` is reached |
|---|---|
| set | the exporter binds it and answers `GET /metrics` itself, with no authentication — bind it internally |
| unset | the handle `init` returns is rendered on the API's own `/metrics` route, which requires the admin role |

Setting `metrics_addr` on the service binds a second port serving the same
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
