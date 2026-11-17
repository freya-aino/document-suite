# vs-api — Specification

The suite's ingress. Your existing front-end service-api calls it. **Stateless**: all state
is in Postgres (`ops`), the object store and the task bus, so it scales horizontally on
request rate, independently of the workers. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) §5.6 (security) and §5.3 (lanes).

## Status

Implemented: every endpoint below, multipart and JSON upload, `Idempotency-Key` replay,
interactive back-pressure (`429` with `Retry-After`), source-URI tenant-prefix checks,
DAG-list admin with `vs_router::Router::load` validation, `/metrics`, and
**authentication against Keycloak** — see [Authentication](#authentication) for what it
verifies and what it still does not. 23 tests pass (19 of them authentication's own,
against real RSA signatures and a real JWKS over a socket), plus 5 HTTP tests
(`src/http_tests.rs`, `#[ignore]`d; they need Postgres) driving the real router over the
real `ops` schema with real signed tokens, an in-memory blob store and a fake bus whose
backlog the test sets.

The header placeholder is gone. There is no configuration in which this binary serves a
request without verifying a token: `[auth]` is required, the process refuses to start
without it, and the verification is one middleware over every route but `/healthz` and
`/readyz` rather than a per-handler check.

Remaining:
- [ ] **Read scoping inside an organization is not enforced.** The isolation boundary is
      the organization, and it holds: it is the tenant, it is hashed into every document
      and entity key, and nothing lets a caller name another one. But *within* an
      organization every member sees every document, while the identity model says
      access should be the person's. The job already records who asked
      (`JobSpec::owner`: subject, department, workspaces, from the verified token), so
      what is missing is the enforcement, and the reason it is not here is that the
      vault's rows carry no owner: `GET /v1/documents/{hk}` could be scoped only by
      adding ownership columns and a migration. Doing it half-way — private jobs, shared
      documents — would read as a working check while the documents stayed open, which
      is worse than a stated gap.
- [ ] No per-tenant rate limiting; a single tenant can still fill the batch lane, now
      with a valid token.
- [ ] Nothing revokes a token before it expires. A token stays good for its `exp` even
      if the session is ended in Keycloak; the realm's token lifetime is the window. No
      introspection call (it would put Keycloak on the path of every request) and no
      deny-list.
- [ ] A local Keycloak is now a prerequisite for running `vs-api` in development.
      `config/worker.dev.toml` carries the `docker run` line for one; there is no devenv
      service for it.
- [ ] Test coverage: the five paths where a regression silently loses or mis-attributes
      work have tests — replay, back-pressure, the batch path, the `source_uri` prefix
      check and DAG-list activation. What is still untested is the multipart upload path
      (`skip_if_processed`, the body limit, the spool file's cleanup) and the read routes,
      which fail loudly rather than quietly.
- [ ] No Kafka bus (inherited from `vs-infra`; nothing needs it yet).

## Authentication

Keycloak-issued OIDC access tokens, as a bearer header. `src/auth.rs`; what it refuses is
`src/auth_tests.rs`.

**The tenant is the organization** (decision D11). It comes from a verified claim —
`auth.org_claim`, default `organization` — and from nowhere else: not a header, not a
body, not the subject. That choice is what keeps this an API change rather than a storage
migration, because `TenantId` is already the key material everywhere (`HashKey::document`,
`HashKey::entity`, the blob key prefix, the Qdrant tenant partition), and it is what makes
one corpus per organization: entity resolution and `skip_if_processed` work across
colleagues, which they could not if the tenant were the person.

What every request is held to, each with a test:

| Check | Why it is there |
|---|---|
| Signature, against the realm's published JWKS | the one check everything else rests on |
| Algorithm from an allowlist of asymmetric algorithms | a JWKS is public, so accepting `HS*` would let anyone sign tokens with the modulus as the HMAC secret |
| `kid` present, and the key's own declared `alg` honoured | "try every key" is how a withdrawn key keeps working |
| `iss` equals `auth.issuer` | a token from another realm is not ours |
| `aud` contains an `auth.audience` entry | Keycloak signs every client in a realm with the same key, so without this a token for an unrelated client opens this API |
| `exp`, and `nbf` (off by default in `jsonwebtoken`), with `auth.leeway_s` of skew | a token outside its own window |
| `typ` is not `Refresh` or `ID` | a refresh token is signed by the same key as an access token |
| `sub` present and non-empty | there is no caller otherwise |
| The organization claim is `[A-Za-z0-9_-]{1,64}` | it becomes a path segment in `originals/<tenant>/…` and a `\|`-joined hash-key part, so `../other` would be a traversal and `a|b` a collision between two organizations |

Answers: `401` with a `WWW-Authenticate: Bearer` challenge for anything wrong with the
token; `403` for a valid token that is not entitled (no organization, or an admin route
without the role) because refreshing it will not help; `503` with `Retry-After` when
Keycloak has never answered, since that is our fault and worth retrying. The caller is
told the class and not which check failed — the detail goes to the log and to the
`reason` label on `vs_api_auth_failures_total`.

**Keys.** OIDC discovery at startup (`<issuer>/.well-known/openid-configuration`, retried
three times, and the document's own `issuer` must match the configured one before its
`jwks_uri` is trusted), then the JWKS, re-read every `auth.jwks_refresh_s`. A `kid` that
is not in the cache provokes at most one fetch per 10 s: an unknown `kid` is either a
rotation or a forgery, and without that floor a stream of junk tokens would make this
endpoint an amplifier against Keycloak. A rotation is therefore picked up without a
restart, which is asserted against a realm served on a real socket.

**Roles.** `realm_access.roles` plus, when `auth.client_id` names one,
`resource_access.<client_id>.roles`. Another client's roles are that client's business.

**What Keycloak needs configured**: an audience mapper on the `vs-api` client (otherwise
Keycloak mints `aud: account` and every request is refused), a mapper putting the
organization in the claim `auth.org_claim` names, and the `auth.admin_role` role. A
Prometheus scrape authenticates as a service account (client credentials) holding that
role.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A19 | The language hint (BCP-47) applies to all pages of a document in v1; it is never detected. |
| — | `doc_type` is an *optional* hint with the same shape: trusted over anything inferred, and the only document-level signal that exists before a page is routed. |
| A20 | Per-call options: `language` (required), `priority`, optional `profile` (`quality`/`balanced`/`fast`), optional DAG-list version pin. |

## Endpoints (v1)

All endpoints are tenant-scoped, and the tenant is the organization claim of a verified
Keycloak token — never a header, never the body. See [Authentication](#authentication).

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

All require the `auth.admin_role` role, as a realm role or on the `auth.client_id`
client. Scope: the calling organization's lists by default, `?global=true` for the global
list (used by organizations without their own).

- `GET /v1/dag-lists` — the tenant's and the global versions with status.
- `POST /v1/dag-lists` — upload a candidate (from `vs-adapt`); validated with
  `vs_router::Router::load` against `vs_steps::production_registry` before it is stored
  (`400` with the validation error, `409` if the version exists, `503` if the model
  registry failed to load at startup — the API still serves everything else).
- `POST /v1/dag-lists/{version}/activate` — makes it active and retires the previously
  active list of the same scope in one transaction.

### Ops

`GET /healthz` (process up) and `GET /readyz` (Postgres + blob reachable) are the only
unauthenticated routes: a load balancer holds no token and neither answer says anything
about a tenant. `GET /metrics` **needs the admin role** — this API is reachable from
outside, where per-route request volumes, upload sizes and error rates are a free read of
how much work every tenant is sending (Prometheus text: `vs_api_http_requests_total` and `vs_api_http_request_duration_seconds`
by method/matched path/status, `vs_api_jobs_created_total` by priority,
`vs_api_backpressure_total`; via the `metrics` crate so other counters can be added
anywhere). Authentication adds `vs_api_auth_failures_total` by `reason` and
`vs_api_jwks_fetches_total` by `outcome`.

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
