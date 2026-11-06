# vs-api — Specification

The suite's ingress. Your existing front-end service-api calls it. **Stateless**: all state
is in Postgres (`ops`), the object store and the task bus, so it scales horizontally on
request rate, independently of the workers.

## Endpoints (v1)

All endpoints are tenant-scoped; the tenant comes from the auth token (mTLS or JWT from
your service-api — to be decided), never from the body.

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

- Behaviour: stream upload to `originals/<tenant>/<sha256>` (hashing while streaming),
  create `ops.jobs` row (`received`), publish `Task::Ingest` (dedup id `<job_id>:ingest`).
- Response `202`: `{ job_id, document_hk, run_id, status_url }`.
- Same file uploaded again by the same tenant → new run of the same `document_hk`
  (reprocessing). `?skip_if_processed=true` returns the existing current run instead.

### `POST /v1/batches`

Many documents in one call: `{ options, documents: [{ source_uri, external_ref?, language? }] }`
(per-document `language` overrides the batch option). Max 10 000 documents per call.
Response `202`: `{ batch_id, job_ids[] }`.

### `GET /v1/jobs/{job_id}`

`{ state, pages_total, pages_done, pages_failed, document_hk, run_id, error?, timings }`.

### `GET /v1/batches/{batch_id}`

Aggregated counts per state + paginated job list.

### `GET /v1/documents/{document_hk}`

Current run summary: pages with profile and DAG used, block counts by kind, entity and
assertion counts. `?run_id=` for older runs.

### `GET /v1/documents/{document_hk}/pages/{page_no}`

The `PageResult` (blocks, provenance) of the current run — for review tools and debugging.

### Admin: DAG lists

- `GET /v1/dag-lists` — versions with status.
- `POST /v1/dag-lists` — upload a candidate (from `vs-adapt`); validated with the
  production step registry before it is stored.
- `POST /v1/dag-lists/{version}/activate` — makes it active (one active per tenant/global).

### Ops

`GET /healthz` (process up), `GET /readyz` (Postgres + bus + blob reachable), `GET /metrics`.

## Non-functional

- Upload streaming with a size limit (`max_upload_bytes`, default 2 GiB).
- Idempotency: `Idempotency-Key` header on `POST /v1/documents` and `/v1/batches`
  (stored 24 h in `ops`) so client retries do not create duplicate jobs.
- Back-pressure: if the interactive lane has more pending messages than
  `interactive_max_pending`, interactive requests get `429` with `Retry-After`;
  batch requests are always accepted (they queue).
- No synchronous model calls in the API.
