# vs-api — Specification

The suite's ingress. Your existing front-end service-api calls it. **Stateless**: all state
is in Postgres (`ops`) and the object store, so a copy keeps nothing another copy needs.

These routes are a **library**, not a binary: the suite deploys as one service
([`vs-suite`](../vs-suite/README.md)), which serves them in the same process as the three
pipeline. There is no queue between them (D13): a route accepts an upload, hands it to
[`Dispatch`](src/lib.rs), and the document is processed by this same process. System spec:
[`docs/SPEC.md`](../../docs/SPEC.md) §5.6 (security and personal data), §5.3 (priority)
and §3 (decisions D12, D13).

## Status

Implemented: every endpoint below, multipart and JSON upload, `Idempotency-Key` replay,
interactive back-pressure (`429` with `Retry-After`), source-URI tenant-prefix checks,
DAG-list admin with `vs_router::Router::load` validation, `/metrics`, **authentication
against Keycloak**, **read scoping inside the organization**, **token revocation**,
**per-organization rate limiting**, and **retention and erasure** for the one place a
person's identifier is stored. **39 tests that need no service** and **13 `#[ignore]`d**
against a real Postgres and a real Keycloak.

There is no configuration in which these routes answer a request without verifying a
token: `[auth]` is required, the process refuses to start without it, and verification is
one middleware over every route but `/healthz`, `/readyz` and back-channel logout — which
carries a logout token signed by the realm instead of a bearer token, and whose signature
*is* its authentication.

Read scoping is in [Access](#access-inside-an-organization) and revocation in
[Authentication](#authentication). What is left is in two groups, and the distinction
matters:

**Needs a deployment, not code.**
- [ ] Nothing here runs against a Keycloak *release* build. The development realm is
      `start-dev` with an in-memory database, which is right for a test and is not how a
      realm is deployed; the suite has never met one behind TLS with a persistent
      database, where the issuer is an https URL and the discovery document comes from a
      reverse proxy.
- [ ] The rate limiter is per process, deliberately, because there is nothing between the
      copies to share a counter — the same reason admission control is per copy (D13). A
      fleet of N copies therefore allows N × `api.tenant_rate_per_s`, which is the number
      to size against. A shared limiter would need Redis or a database round trip on the
      hot path, costing more than the requests it sheds.

**Known shape, deliberately not built.**
- [ ] Revocation is a list this API keeps, refreshed every `api.revocation_refresh_s`
      (30 s by default), and *that interval is the window* in which a revoked token still
      works on a copy that has not refreshed. The alternative is token introspection,
      which puts Keycloak on the path of every request — the thing local signature
      verification exists to avoid (D10). The window is stated rather than hidden, and
      it is configurable.
- [ ] Test coverage: the paths where a regression silently loses or mis-attributes work
      have tests — replay, back-pressure, the batch path, the `source_uri` prefix check,
      DAG-list activation, and now every read-scoping decision. What is still untested is
      the multipart upload path (`skip_if_processed`, the body limit, the spool file's
      cleanup), which fails loudly rather than quietly.

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

**What reaches telemetry.** The `tenant` — an organization — on the request span, and
nothing that identifies a person: no subject, no token id, in no span, log line or metric
label (`docs/SPEC.md` §5.6). `Principal`'s `Debug` redacts the subject so that printing one
cannot undo this, and `telemetry_carries_no_user_specific_identifier` captures a real
request's spans and events and fails if either appears. An authentication failure is
counted by `reason` and logged by class; *which* token failed is Keycloak's to record.

**Roles.** `realm_access.roles` plus, when `auth.client_id` names one,
`resource_access.<client_id>.roles`. Another client's roles are that client's business.

**Revocation.** A verified signature says the realm issued the token. It does not say the
session behind it still exists, and until there was a list, a token stayed good for its
`exp` whatever had happened to it. Three kinds of revocation, by what they name:

| Kind | Means |
|---|---|
| `jti` | that one token |
| `sid` | that session, absolutely — a new login gets a new `sid`, so there is nothing an instant would separate |
| `subject` | every token of that person issued before an instant, which is what "log me out everywhere" means without refusing the login they make a second later |

Written by **Keycloak itself**, through `POST /v1/auth/backchannel-logout` (OpenID Connect
Back-Channel Logout 1.0): unauthenticated by design, because the request carries a logout
token signed by the realm rather than a bearer token, and that signature is the
authentication. The token is held to the realm's keys, its issuer, its audience, the
`events` claim naming the logout event, the absence of a `nonce` (§2.6 — an ID token has
one, and accepting it would turn every login into a logout) and a freshness bound on
`iat`. Also written by `POST /v1/auth/revocations`, where an administrator may revoke
anything in the organization and anyone may revoke their own session or subject.

Read from an in-process cache refreshed every `api.revocation_refresh_s`. A token with no
`iat` is refused by a subject-wide revocation rather than let through: the question
"issued before the logout?" cannot be answered without an issue time, and the safe reading
of an unanswerable question is the restrictive one.

**Development realm.** `deploy/keycloak/` holds the realm the `stack` profile imports
(`devenv --profile stack up`), with `vs-token` to fetch a token from it; `vs-api` cannot
start without a reachable realm, so it is part of the profile rather than an extra. The
`#[ignore]`d `a_token_from_the_development_realm_verifies` asks that realm for a token and
asserts the claims arrive as this verifier expects, which is what notices if the realm
file and the code drift apart.

**Test keys.** The rest of the authentication tests sign their own tokens with the two RSA
keypairs in `fixtures/`, committed deliberately: they are throwaway material, trusted by
nothing, and having them lets every test above verify a real signature instead of mocking
the verifier out. `fixtures/README.md` says what each one is and how to regenerate it. A
secret scanner may flag the PEMs.

**What Keycloak needs configured**: an audience mapper on the `vs-api` client (otherwise
Keycloak mints `aud: account` and every request is refused), a mapper putting the
organization in the claim `auth.org_claim` names, and the `auth.admin_role` role. A
Prometheus scrape authenticates as a service account (client credentials) holding that
role.

## Access inside an organization

The organization is the isolation boundary and always was (D11): it is the tenant, it is
hashed into every document and entity key, it prefixes every blob key, and nothing lets a
caller name another one. What did not exist until now is scoping *within* one — every
member of an organization could read every document it owned, while the identity model
said access should be the person's.

**Where the rows live, and why not in the vault** (decision D14). The vault is
insert-only by design (D5) and insert-only at the database level: migration 0003 grants
the runtime role neither UPDATE nor DELETE on it. A share could therefore not be
withdrawn there and an erasure request could not be served there. Access is mutable
state, so it lives in `ops`, beside the jobs — which is also the only other place a
person's identifier is stored, so there is one surface to erase rather than two.

**The rule** is `vs_vault::access::Reader::may_read`, and it is one function. A caller may
read a document when any of these holds:

| | |
|---|---|
| they hold `auth.admin_role` and `api.admin_reads_all` is on (the default) | an operator who cannot read a document cannot answer a question about it |
| they are its owner | whatever its visibility |
| nothing recorded an owner | what every document uploaded before scoping existed looks like; inventing an owner would lock a tenant out of its own history |
| `visibility = organization` | what every document used to be |
| `visibility = department` and both sides name the same department | two people with *no* department are not colleagues, so a missing claim matches nothing |
| `visibility = workspace` and they share one of the document's workspaces | snapshotted at upload, so a share survives the uploader leaving the workspace |
| a grant names them, their department or one of their workspaces | the named exception |

`api.default_visibility` decides what a new upload is, and it defaults to **private** —
the identity model's answer. A deployment that wants the old behaviour sets
`organization` and gets exactly it.

The listing route (`GET /v1/documents`) narrows in SQL and then puts every row through
the same `may_read`: two implementations of one rule is a divergence waiting to happen,
so only one of them decides. A document's hash key is derived from its content's SHA-256,
so a caller who can name one already has the bytes — a refusal is therefore `403` with an
explanation, not a `404` pretending the document is not there.

A **job** is the asker's, plus anyone who can read the document it produced, plus an
administrator. A job that has not produced a document yet is the asker's alone; there is
nothing to share.

**Erasure.** `DELETE /v1/owners/{subject}`, allowed to an administrator and to the person
themselves — an erasure request is the data subject's to make, and requiring a relay would
be a worse answer. It clears the subject from the job rows and from `spec.owner`, clears
the owner from the documents and widens them to the organization (a private document whose
owner no longer exists would be readable by nobody), and deletes every grant to or by that
person. It touches no satellite, because no satellite stores an owner — which is what makes
it short.

**Retention.** `api.job_retention_days` (90 by default): the caller's subject is scrubbed
from finished jobs older than that, every six hours. The subject is stored because
per-person access control needs it *while the work is live*; afterwards it is a personal
identifier with no remaining purpose, which is what storage limitation (GDPR Art. 5(1)(e))
is about. The document's access row keeps its owner, because that is still in use.

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
  (`received`, `document_hk` already set) and hand it to the pipeline in this process
  (D13 — there is no queue to publish to). The JSON variant leaves `document_hk` to
  ingest.
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

### Access and sharing

| Route | Who | What |
|---|---|---|
| `GET /v1/documents` | anyone | the documents this caller may read, newest first (`page`, `page_size`) |
| `GET /v1/documents/{hk}/access` | anyone who may read it | visibility, workspaces and grants; the owner's subject only to the owner and to an administrator |
| `PUT /v1/documents/{hk}/access` | owner or administrator | `{ "visibility": "private" \| "department" \| "workspace" \| "organization" }` |
| `POST /v1/documents/{hk}/grants` | owner or administrator | `{ "kind": "subject" \| "department" \| "workspace", "id": "…" }`; granting twice is one grant |
| `DELETE /v1/documents/{hk}/grants/{kind}/{id}` | owner or administrator | withdraws it |
| `DELETE /v1/owners/{subject}` | administrator, or that person | erasure; answers what it removed, per table |

Reading a document and re-sharing it are different powers: `api.admin_reads_all` governs
the first and never the second, so a deployment that separates administration from access
still has someone able to fix a mis-shared document.

### Authentication: revocation

| Route | Who | What |
|---|---|---|
| `POST /v1/auth/backchannel-logout` | the realm, by signature | OpenID Connect Back-Channel Logout 1.0; `logout_token` as a form field |
| `POST /v1/auth/revocations` | administrator, or the owner of the token/session/subject | `{ "kind": "jti" \| "sid" \| "subject", "id": "…", "reason": "…" }` |
| `GET /v1/auth/revocations` | administrator | how many revocations this copy holds, and its refresh interval |

The status route answers a **count**, not a list: the list is subjects and session ids,
which is personal data this API has no reason to hand out, and the operational question is
whether revocation is working at all.

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
- Back-pressure: if this process is already carrying more than
  `api.interactive_max_pending` documents (`Dispatch::in_flight`), interactive requests get
  `429` with `Retry-After: 30`; batch requests are always accepted. The check runs before
  any state is written. The depth is per copy now rather than a queue every copy could
  see — which is also where the work will run, so it is the number that matters, but it
  does mean a load balancer sees a `429` from one copy and not from another.
- Per-organization rate limiting: a token bucket at `api.tenant_rate_per_s` with
  `api.tenant_rate_burst`, after authentication because the organization comes from the
  verified token. Over it, `429` with a `Retry-After` of at least one second, so a client
  that obeys it is not refused again immediately. **Per process**, like admission control
  and for the same reason (D13): a fleet of N copies allows N × the rate, which is the
  number to size against. `0` disables it.
- No synchronous model calls in the API.
