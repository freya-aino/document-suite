# vs-api — Specification

The suite's ingress. Your existing front-end service-api calls it. **Stateless**: all state
is in Postgres (`ops`), the object store and the task bus, so it scales horizontally on
request rate, independently of the workers.

## Endpoints (v1)

All endpoints are tenant-scoped; the tenant comes from the auth token (mTLS or JWT from
your service-api — to be decided), never from the body.

**Current state:** until auth is decided, the tenant is read from the `X-VS-Tenant`
header (`[A-Za-z0-9_-]+`), which any caller can set. Keep the API on a trusted network
until this is replaced.

Admin routes (DAG lists) additionally require `X-VS-Admin: 1` — the same kind of
placeholder until roles come from the auth token.

Implemented: every endpoint below, multipart and JSON upload, `Idempotency-Key`,
interactive back-pressure (`429`), `/metrics`. Not yet: auth (tenant and admin come from
headers), Kafka bus.

### `POST /v1/documents`

Upload one document.

- Body: `multipart/form-data` with `file` and `options` (JSON), **or** JSON with
  `source_uri` pointing to an object already in the bucket (preferred for large files).
- `options`:

  | Field | Required | Default | Notes |
  |-------|----------|---------|-------|
  | `language` | yes | — | BCP-47 hint, applies to all pages (A19) |
  | `priority` | no | `batch` | `interactive` for small batches |
  | `profile` | no | `balanced` | `quality` / `balanced` / `fast` (A20) |
  | `dag_list_version` | no | active | pin for reproducibility |
  | `external_ref` | no | — | your id, stored in `sat_document_meta` |

- `source_uri` must point under `originals/<tenant>/` or `inbox/<tenant>/` (as
  `s3://<bucket>/<key>` or a bare key), so a tenant cannot submit another tenant's
  files; the object must exist (`400` otherwise).
- Behaviour: the file part is hashed while being spooled to `api.scratch_dir` (the blob
  key is its hash, so it cannot go to the store directly), then multipart-uploaded to
  `originals/<tenant>/<sha256>` unless that object exists; create the `ops.jobs` row
  (`received`, `document_hk` already set), publish `Task::Ingest` (dedup id
  `<job_id>:ingest`). The JSON variant leaves `document_hk` to ingest.
- Response `202`: `{ job_id, document_hk, run_id, status_url }`; `document_hk` is `null`
  for the JSON variant unless `skip_if_processed` made the API hash the object.
- Same file uploaded again by the same tenant → new run of the same `document_hk`
  (reprocessing). `?skip_if_processed=true` returns the tenant's current (newest,
  non-failed) job for that document with `200` instead.
- Errors: `413` above `api.max_upload_bytes`, `400` for a missing `options`/`file` part.

### `POST /v1/batches`

Many documents in one call: `{ options, documents: [{ source_uri, external_ref?, language? }] }`
(per-document `language` overrides the batch option, which is then optional). Max 10 000
documents per call. Every `source_uri` is checked (tenant prefix, existence) before any
state is written; then one `ops.batches` row, all jobs in one insert, ingest tasks
published 32 at a time. Jobs whose task could not be published are marked `failed` and
listed. Response `202`: `{ batch_id, job_ids[], failed_job_ids[], status_url }`.

### `GET /v1/jobs/{job_id}`

`{ state, pages_total, pages_done, pages_failed, document_hk, run_id, error?, created_at,
updated_at, timings }` — `timings.states[]` is when the job entered each state
(`ops.job_state_log`).

### `GET /v1/batches/{batch_id}`

Aggregated counts per state + paginated job list (`?page=0&page_size=100`, max 1000).

### `GET /v1/documents/{document_hk}`

Current run summary read from the vault: run state and DAG-list version, pages with
profile, DAG and routing rule, block counts by kind, entity and assertion counts.
`?run_id=` for older runs. The document must belong to the tenant (`hub_document`),
otherwise `404`.

### `GET /v1/documents/{document_hk}/pages/{page_no}`

The `PageResult` (blocks, provenance) of the current run (or `?run_id=`), streamed from
the blob at `results/<document_hk>/<run_id>/page-<n>.json` — for review tools and
debugging. `404` while the page is unprocessed.

### Admin: DAG lists

All require `X-VS-Admin: 1`. Scope: the calling tenant's lists by default,
`?global=true` for the global list (used by tenants without their own).

- `GET /v1/dag-lists` — the tenant's and the global versions with status.
- `POST /v1/dag-lists` — upload a candidate (from `vs-adapt`); validated with
  `vs_router::Router::load` against `vs_steps::production_registry` before it is stored
  (`400` with the validation error, `409` if the version exists, `503` if the model
  registry failed to load at startup — the API still serves everything else).
- `POST /v1/dag-lists/{version}/activate` — makes it active and retires the previously
  active list of the same scope in one transaction.

### Ops

`GET /healthz` (process up), `GET /readyz` (Postgres + blob reachable), `GET /metrics`
(Prometheus text: `vs_api_http_requests_total` and `vs_api_http_request_duration_seconds`
by method/matched path/status, `vs_api_jobs_created_total` by priority,
`vs_api_backpressure_total`; via the `metrics` crate so other counters can be added
anywhere).

## Non-functional

- Upload streaming with a size limit (`api.max_upload_bytes`, default 2 GiB; `413`).
- Idempotency: `Idempotency-Key` header on `POST /v1/documents` and `/v1/batches`. The
  `{status, body}` of a successful response is stored in `ops.idempotency_keys` for 24 h
  (per tenant and route) and replayed with the same status and an
  `Idempotent-Replayed: true` header. Two *concurrent* first requests with the same key
  both run (no lock); the API purges expired keys hourly.
- Back-pressure: if the interactive ingest lane has more pending messages than
  `api.interactive_max_pending` (JetStream `num_pending`), interactive requests get
  `429` with `Retry-After: 30`; batch requests are always accepted (they queue). The
  check runs before any state is written.
- No synchronous model calls in the API.
