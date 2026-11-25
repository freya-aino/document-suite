# Embedding the suite in another deployment

The default devenv shell builds the suite and **starts nothing**: no Postgres, no S3, no
Qdrant, no Keycloak. That is deliberate. Another project embedding this one wants the
binaries, not this repo's idea of where a database should live, so every endpoint is
configuration — and the local stack is a profile you can ignore:

```sh
devenv shell                      # toolchain, pdfium, sqlx-cli — no services
cargo build --release -p ds-suite
docker build -f deploy/docker/Dockerfile -t document-suite:dev .   # or as an image
devenv --profile stack up -d      # only if you want this repo's services too

cargo build --release -p ds-mcp   # the optional LLM tool service
docker build -f deploy/docker/Dockerfile.mcp -t document-suite-mcp:dev .
```

## The service

**One binary, one process, one service.** `ds-suite` serves the HTTP API and processes the
documents it accepts: ingest, then the pages concurrently, then Stage 2, in the same
process. A deployment adds one unit and scales it by running more copies, each taking
whole documents.

| | |
|---|---|
| Listens | `DS_API_ADDR`, default `0.0.0.0:8080` |
| Needs | Postgres, S3, Qdrant, Keycloak and model endpoints — all of them, in every copy |
| Stop | SIGTERM: stops accepting requests, then gives in-flight documents `--shutdown-grace` (60 s) |

There is **no queue**. That is decision D13: the per-page DAG always ran in-process, so a
bus only spread one document's pages across machines, at the price of a service to run and
a redelivery protocol to maintain. One consequence remains and is worth knowing before you
plan capacity: **a single large document is as fast as one copy can make it.**

The other consequence is gone. A crash no longer abandons the documents a copy was
carrying: each job is leased by the process working on it, a lease that stops being
renewed is what a crash looks like from outside, and every copy sweeps for lapsed leases
and picks those documents up (`worker.resume_interval_s`). Re-running resumes rather than
repeats — rendered pages are skipped and vault writes are idempotent — and
`worker.max_resumes` stops a document that kills the process reading it from taking down
the fleet one copy at a time.

Two other invocations of the same binary are not services: `ds-suite migrate` is a deploy
step (schema **owner** credentials, not the runtime role's), and `ds-suite render-child` is
what the ingest stage re-executes per render — you never run it yourself.

### Scaling it

More copies. Each one takes whole documents, so request capacity and document throughput
grow together; within a copy, `worker.max_documents` bounds documents in flight and
`worker.max_in_flight` bounds pages across them, with `worker.interactive_share` reserved
for uploads sent as `priority = interactive`.

What you cannot do any more is scale the halves separately — there are no roles to split,
and `--roles` is gone. Nor can you spread one document over several copies.

Two settings are about *other* copies rather than this one: `worker.lease_ttl_s` is how
long this copy's claim on a document is good for without a renewal, and
`worker.resume_interval_s` is how often it looks for documents another copy abandoned. Set
the interval to 0 and nothing recovers them, which is what happened before leases existed.

`ds-adapt` is the other binary and is never deployed: an offline tool for scoring
candidate workflows in dev and stage.

### The image

[`docker/Dockerfile`](docker/Dockerfile) builds the service. Build it from the repository
root — the context is the workspace, because the build compiles it:

```sh
docker build -f deploy/docker/Dockerfile -t document-suite:dev .
```

| Stage | What it does |
|---|---|
| `pdfium` | fetches the prebuilt `libpdfium.so`, pinned by release **and** SHA-256 |
| `build` | `cargo build --release --locked -p ds-suite` on `rust:1.100-bookworm` |
| `runtime` | `debian:bookworm-slim`, the binary, the library, `ca-certificates`, and a minimal config |

Three things about it worth knowing before you change them:

- **pdfium is pinned to 8066, not "latest".** That is the version nixpkgs gives the dev
  shell and CI, so a page renders in the image exactly as it does in the tests. The
  release and its digest are build arguments (`PDFIUM_RELEASE`, `PDFIUM_SHA256`), so
  moving it is deliberate and verified rather than silent.
- **The builder is the newest stable, pinned to an exact release.** The policy is to
  track newest (`rust-version` in the workspace `Cargo.toml`); pinning the release here is
  what keeps a given image reproducible. It may sit ahead of `rust-version`, which is
  capped by what the dev shell provides.
- **The `HEALTHCHECK` probes `/healthz`** unconditionally, because every copy serves the
  API. It used to branch on `DS_ROLES` for copies that only drained the queue.

It runs as uid 10001, writes to `/scratch`, and ships `/etc/ds/worker.toml` as the
smallest valid config: `[models]` pointing at `/etc/ds/models.toml`, and JSON logs.
`[bus]`, `[blob]`, `[postgres]` and `[qdrant]` are deliberately absent — there is no
sensible default for someone else's infrastructure, so the container stops with
`missing field <section>` until they are set, which is better than coming up pointed at
nothing. Mount your own file over `/etc/ds/worker.toml`, or set everything from the
environment; both work.

```sh
docker run --rm -p 8080:8080 \
  -v /path/to/models.toml:/etc/ds/models.toml:ro \
  -e DS__BLOB__BUCKET=document-suite -e DS__BLOB__ENDPOINT=https://s3.internal \
  -e DS__POSTGRES__URL=postgres://ds_app@pg:5432/document_suite -e DS__POSTGRES__PASSWORD=… \
  -e DS__QDRANT__URL=http://qdrant:6334 \
  -e DS__AUTH__ISSUER=https://sso.example.org/realms/koki -e DS__AUTH__AUDIENCE=ds-api \
  -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… -e AWS_REGION=… \
  document-suite:dev
```

`docker run … document-suite:dev migrate --database-url …` runs the deploy step from the
same image, as the schema owner.

## The LLM tool service, if you want it

`ds-mcp` serves the same work as **Model Context Protocol tools**, so an agent or an
assistant can submit documents, follow jobs, read text and manage sharing. It is optional
and entirely separate: a deployment that wants the API alone runs one fewer binary and
nothing else changes. Full detail in [`crates/ds-mcp/SPEC.md`](../crates/ds-mcp/SPEC.md).

The one thing worth knowing before deploying it: **it holds no credentials** (decision
D16). Every tool call carries the token of whoever is calling, and there is no
service-account fallback — so a model reaches exactly what the person driving it can reach,
and nothing more. There is no organization or subject argument on any tool.

Two shapes, and the choice is about who starts the process:

```sh
# A service. Every request carries its own caller's token, from its Authorization header.
docker run --rm -p 8081:8081 \
  -e DS_API_URL=https://suite.internal \
  document-suite-mcp:dev http --allowed-origin https://app.internal

# One process per user, launched by their client. The token is theirs, from their
# environment — which is why this form is a binary and not a container.
DS_API_TOKEN_FILE=/run/secrets/ds-token ds-mcp --api https://suite.internal stdio
```

In HTTP mode the clients are MCP clients, which means **your** authorization layer is what
issues those tokens: the server verifies nothing itself, the suite does. Bind it where the
suite's own API is reachable, and treat `--allowed-origin` as required rather than optional
if anything browser-based will call it — without it, a request carrying an `Origin` header
is refused, which is what stops a web page from using the server as the user.

What it will not do, by design: erase a person's documents, revoke a token or a session, or
change DAG-list routing. Those routes exist on the API and are deliberately not tools —
a model should not be able to erase somebody because a document's text asked it to.

There is **no compose file and no chart** here: how the service is wired to your Postgres,
your Qdrant and your realm is your deployment's business, and this repo guessing at it would
be a file nobody can use unchanged. `deploy/firecracker/build-rootfs.sh` is the other
packaging that does exist, for microVMs.

## Configuration

Three layers, in order of precedence: the environment overrides the config file, and the
config file overrides the defaults below.
[`config/worker.example.toml`](../config/worker.example.toml) is the same surface as an
annotated file to copy; below is the complete list, with every default.

### Process parameters

| Flag | Environment | Default |
|---|---|---|
| `--config` | `DS_CONFIG` | `/etc/ds/worker.toml`; the file must exist, even if empty |
| `--addr` | `DS_API_ADDR` | `0.0.0.0:8080` |
| `--shutdown-grace` | `DS_SHUTDOWN_GRACE` | `60` seconds to drain on SIGTERM |
| `migrate --database-url` | `DATABASE_URL` | —; the deploy step, as the schema **owner** |

Ports: `8080` inbound, plus `telemetry.metrics_addr` if set.

### The config file

**Required sections**: `[blob]`, `[postgres]`, `[qdrant]`, `[models]`. A missing
one is a startup error (`Error: missing field 'qdrant'`). `[qdrant]` is required even for a
copy that never reaches Stage 2, on purpose: a misconfigured deployment should fail at
startup, not at the first document that gets that far.

**Optional sections**: `[worker]`, `[api]`, `[telemetry]`, `[classify]`, `[stage2]`,
`[blob.retention]`, and `[auth]` — which the service then requires at runtime.

| Key | Default | |
|---|---|---|
| `blob.bucket` | **required** | nothing creates it |
| `blob.endpoint` | none | an `http://` endpoint enables plain HTTP by itself |
| `blob.region` | none | |
| `blob.sse` | `none` | server-side encryption: `none`, `aes256`, `aws:kms`, `aws:kms:dsse`, `sse-c`. A mode whose key is missing fails at startup rather than writing plaintext |
| `blob.sse_kms_key_id` | none | required by the two KMS modes |
| `blob.sse_customer_key_env` | none | the *name* of the variable holding the base64 SSE-C key |
| `blob.retention.page_images_days` | `30` | rendered pages and thumbnails; rebuildable by re-running the document |
| `blob.retention.results_days` | `90` | Stage 1 and Stage 2 result JSON |
| `blob.retention.text_layers_days` | `30` | extracted text layers; rebuildable |
| `blob.retention.crops_days` | `7` | block crops handed to models |
| `blob.retention.model_images_days` | `1` | scratch images for `image_transport = "url"` |
| `blob.retention.originals_days` | `0` | **forever**: deleting a tenant's input is not this software's decision |
| `blob.retention.sweep_interval_h` | `6` | 0 disables the sweep entirely |
| `postgres.url` | **required** | without the password |
| `postgres.password` | none | from the environment only |
| `postgres.max_connections` | `16` | per **process**, shared by the API and every stage in it |
| `qdrant.url` | `http://127.0.0.1:6334` | the **gRPC** port; 6333 is REST and will not work |
| `qdrant.api_key_env` | none | the *name* of the variable holding the key |
| `qdrant.collection_prefix` | `ds` | lets one Qdrant host several deployments. Changing it does not move data: the suite creates the new collections empty and the old ones keep the documents, which reads as though everything vanished. The same is true of `blob.bucket`. Both defaults changed with the suite's rename, so an environment populated under the old names must either keep the old values or be re-ingested |
| `qdrant.batch_size` | `256` | points per upsert |
| `qdrant.idf` | `true` | whether Qdrant applies IDF to the lexical half. The statistics are the collection's, so they span organizations — set false for a deployment that will not have one organization's ranking depend on another's corpus |
| `models.registry` | **required** | path to the model-registry TOML |
| `auth.issuer` | **required** | matched against `iss` exactly; the base for OIDC discovery |
| `auth.audience` | **required** | a list, or one comma-separated string |
| `auth.client_id` | none | whose `resource_access.<id>.roles` are read as well as realm roles |
| `auth.org_claim` | `organization` | **becomes the tenant**; a dotted path reads a nested claim |
| `auth.department_claim` | `department` | recorded on the job, and what `visibility = department` is evaluated against |
| `auth.workspaces_claim` | `workspaces` | recorded on the job, and what `visibility = workspace` is evaluated against |
| `auth.admin_role` | `ds-admin` | the DAG-list routes and `/metrics` |
| `auth.jwks_refresh_s` | `300` | how long a fetched JWKS is reused |
| `auth.leeway_s` | `30` | clock skew allowed on `exp` and `nbf` |
| `api.max_upload_bytes` | `2147483648` | 413 above it |
| `api.interactive_max_pending` | `1000` | 429 for interactive above this many documents in flight |
| `api.scratch_dir` | system temp | upload spool while hashing |
| `api.default_visibility` | `private` | how wide a new upload is open: `private`, `department`, `workspace`, `organization`. `organization` is what the API did before per-person scoping |
| `api.admin_reads_all` | `true` | whether the admin role implies reading every document in the organization. Never implies *re-sharing* one |
| `api.tenant_rate_per_s` | `50` | requests per second per organization, **per process**; 0 disables |
| `api.tenant_rate_burst` | `100` | requests allowed at once before the rate applies |
| `api.job_retention_days` | `90` | after this, the caller's subject is scrubbed from finished jobs; 0 keeps it forever |
| `api.revocation_refresh_s` | `30` | how often the revocation list is re-read — and therefore the window in which a revoked token still works on a copy |
| `worker.max_in_flight` | `32` | pages at once across every document this copy is running |
| `worker.interactive_share` | `0.3` | page slots reserved for interactive-priority documents |
| `worker.render_dpi` | `200` | |
| `worker.max_failed_page_fraction` | `0.05` | above it the document fails |
| `worker.max_documents` | `4` | documents in flight in this process; each holds a render child and a spooled original |
| `worker.page_attempts` | `3` | in-process retries of a page on a *transient* failure; replaces the queue's `max_deliver`, with a 2/4/8 s backoff |
| `worker.scratch_dir` | system temp | where ingest spools originals |
| `worker.render_processes` | available CPUs | render children, shared by ingest and by `crop_dpi` re-renders |
| `worker.max_crop_dpi` | `600` | finest DPI a DAG may ask for with `crop_dpi`; 0 disables re-rendering |
| `worker.lease_ttl_s` | `120` | how long this copy's claim on a document is good for without a renewal |
| `worker.resume_interval_s` | `60` | how often this copy looks for documents a crashed copy abandoned; 0 disables recovery |
| `worker.resume_batch` | `4` | abandoned documents claimed per sweep |
| `worker.max_resumes` | `3` | times a document may be picked up before its job is failed |
| `worker.page_render_timeout_s` | `60` | a slower page fails its document permanently |
| `worker.render_mem_limit_mb` | none | address-space cap per render child |
| `telemetry.otlp_endpoint` | none | OTLP over **HTTP** (4318, not gRPC's 4317); bare origin, `/v1/traces` is appended |
| `telemetry.metrics_addr` | none | **unauthenticated** — bind an internal interface, never `0.0.0.0`; leave it unset to scrape the API's own `/metrics`, which needs the admin role |
| `telemetry.json_logs` | `false` | |
| `telemetry.sample_ratio` | `1.0` | fraction of traces exported, parent-based so a trace is never exported in pieces. 0.0 is an off switch |
| `stage2.extraction_model` | `extract-llm` | ids from `models.registry`, not URLs |
| `stage2.verify_model` | `extract-llm` | may equal the extraction model |
| `stage2.embedding_model` | `embed-text` | |
| `stage2.judge_model` | none | unset = thresholds decide corpus resolution alone |
| `stage2.doc_classifier_model` | none | unset = the document type is the weighted page vote, and pages route on their own class. Set, it is called **once per document during ingest** so every page is routed knowing what document it belongs to |
| `stage2.doc_classifier_pages` | `3` | opening pages shown to that classifier |
| `stage2.classifier_model` | none | unset = rules-only page classification |
| `stage2.window_chars` | `12000` | characters per extraction window |
| `stage2.type_match_threshold` | `0.85` | cosine, for mapping raw types onto the ontology |
| `stage2.link_threshold` | `0.92` | links an entity without asking the judge |
| `stage2.judge_band` | `[0.80, 0.92]` | the band the judge decides in — an array, so **not** settable by environment |
| `stage2.embed_batch_size` | `0` | 0 = the registry's per-model batch size |
| `stage2.dag_list_refresh_s` | `60` | how long a copy trusts its cached active DAG list |
| `classify.*` | see `ClassifyConfig::default` | `born_digital_min_chars`, `born_digital_min_coverage`, `min_image_area_ratio`, `trust_ocr_layer`, `skip_vision_for_text_only_digital`, `flag_threshold`, `rules_only_confidence`, `thumbnail_long_side_px`, `blank_ink_coverage`, `ruled_min_lines`, `photo_min_tone_continuity`, `unknown_doc_type_confidence` |

### Overriding from the environment

`DS__<SECTION>__<KEY>`, upper case, `__` between levels — which is how secrets stay out of
the file. Three things to know:

- **Scalars only.** An override parses to an integer, float, bool or string and never to
  an array, so `auth.audience` accepts a comma-separated string for exactly this reason,
  and `stage2.judge_band` cannot be set this way at all.
- **A required section needs at least one key from somewhere.** An override creates its
  parent table, so `DS__QDRANT__URL=…` satisfies `[qdrant]` on its own; set nothing for a
  required section and the process stops with `missing field`.
- **An almost-empty file works.** A zero-byte TOML plus the variables below is a complete
  configuration — useful when the whole deployment is environment-driven.

Read outside that scheme:

| | |
|---|---|
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT`, `AWS_ALLOW_HTTP` | S3 credentials and endpoint, through `AmazonS3Builder::from_env()`. `blob.endpoint` and `blob.region` are applied afterwards, so they win |
| `PDFIUM_DYNAMIC_LIB_PATH` | the directory holding `libpdfium.so`; nothing renders without it |
| `DS_HOLDER_ID` | overrides this process's lease holder id. For a restored Firecracker snapshot, where a *random* id would be a copy of the snapshotted process's — see `firecracker/SPEC.md` |
| whatever `qdrant.api_key_env` and each model's `api_key_env` name | the keys themselves |
| `RUST_LOG` | tracing filter, e.g. `info,ds_=debug` |
| `SSL_CERT_FILE`, `TMPDIR` | CA bundle, scratch location |
| `DATABASE_URL` | the `migrate` subcommand only |

### The minimum for the whole service

```sh
DS_CONFIG=/etc/ds/worker.toml            # may be empty
DS__BLOB__BUCKET=document-suite
DS__BLOB__ENDPOINT=https://s3.internal
DS__POSTGRES__URL=postgres://ds_app@pg:5432/document_suite
DS__POSTGRES__PASSWORD=…
DS__QDRANT__URL=http://qdrant:6334
DS__MODELS__REGISTRY=/etc/ds/models.toml
DS__AUTH__ISSUER=https://sso.example.org/realms/koki
DS__AUTH__AUDIENCE=ds-api
AWS_ACCESS_KEY_ID=…  AWS_SECRET_ACCESS_KEY=…  AWS_REGION=…
PDFIUM_DYNAMIC_LIB_PATH=/usr/lib
```

The `DS__AUTH__*` pair is not optional: without it the process stops with
`ds-suite needs an [auth] section in …`. There is no unauthenticated mode.

### What must exist before it starts

1. **An `[auth]` section**, with a reachable issuer. The service fails at startup
   otherwise — there is no unauthenticated mode. Your realm needs an audience mapper, an
   organization claim and the admin role, including on the service account your Prometheus
   scrape uses; see [`keycloak/README.md`](keycloak/README.md). For revocation to be
   driven by the realm rather than by hand, point Keycloak's **back-channel logout URL**
   at `POST /v1/auth/backchannel-logout`; it carries a signed logout token and needs no
   credentials of its own.
2. **Migrations applied, and the `ds_app` role existing.** `ds-suite migrate` runs as the
   schema owner; the service connects as the runtime role `ds_app` (insert-only on the
   vault, granted by migration 0003), which the migrations expect to exist already. The
   service *checks* the schema version at startup and refuses an unmigrated database.
   A database that was migrated under the suite's former name needs its migration
   checksums re-stamped once before either will proceed, and its roles renamed
   (`vs`/`vs_app` → `ds`/`ds_app`); both steps are in
   [`crates/ds-vault/SPEC.md`](../crates/ds-vault/SPEC.md) under "Roles and migrations".
3. **A bucket that exists**, with credentials that can read and write it. Nothing here
   creates it. Qdrant collections, by contrast, are created on first use — behind an alias
   named for the embedder's dimension, so a later dimension change is
   `ds-suite vectors migrate` rather than a manual rebuild.
4. **A decision about `api.default_visibility`.** It defaults to `private`: a new upload
   is readable by its uploader and by an administrator, and nobody else until it is
   shared. If your callers expect what the API did before — every member of an
   organization reading everything it owns — set `organization` deliberately rather than
   discovering the difference from a support ticket.

## Operating it

- **Health**: `GET /healthz` (process up) and `GET /readyz` (Postgres and the blob store
  reachable) need no token, for load balancers. So does
  `POST /v1/auth/backchannel-logout`, whose authentication is the realm's signature on the
  token it carries.
- **Metrics**: `GET /metrics` needs the admin role — scrape it as a service account. Or
  set `telemetry.metrics_addr` and scrape that instead; that port has no authentication,
  so bind it to an internal interface and never `0.0.0.0`. Setting both gives you both.
- **Scaling**: more copies of the service. Within a copy every stage shares one Postgres
  pool, so `postgres.max_connections` is a per-process budget.
- **Tenancy**: the tenant is the organization claim of the caller's token. It is hashed
  into every document and entity key, prefixes every blob key, and partitions the Qdrant
  collections, so **changing what the claim contains re-keys the data** — pick it once.
- **Access inside an organization**: a document is private to its uploader by default and
  shared per document (`PUT /v1/documents/{hk}/access`, `POST …/grants`). The rule is one
  function, the routes are in [`ds-api`](../crates/ds-api/SPEC.md), and the rows are in
  `ops` rather than the vault because a share has to be withdrawable (decision D14).
- **Personal data**: telemetry has none — no span field, log line or metric label
  identifies a person, which is asserted by a test rather than a convention
  (`docs/SPEC.md` §5.6). What holds it is `ops`: the caller's subject on the job and the
  uploader on the document's access row. The Data Vault holds none of it, which is what
  makes an erasure request answerable at all.
  **Retention and erasure are implemented**: `DELETE /v1/owners/{subject}` (an
  administrator, or that person) clears the subject from the jobs, clears the owner from
  their documents and widens those to the organization, and deletes every grant to or by
  them; `api.job_retention_days` scrubs the subject from finished jobs on its own. What is
  still yours to decide is the *policy* — how long, and whether a deployment subject to
  GDPR wants a shorter window than 90 days.
- **Object retention**: `[blob.retention]` prunes derived objects — page images, results,
  crops, the scratch images handed to models — every `sweep_interval_h`. Originals are
  kept forever unless you say otherwise.
- **Reading the stores** directly (the RAG service's job) is
  [`docs/STORES.md`](../docs/STORES.md).

## What is in this directory

| | |
|---|---|
| [`docker/`](docker/Dockerfile) | the service image: multi-stage build, pinned pdfium, minimal baked config |
| [`docker/Dockerfile.mcp`](docker/Dockerfile.mcp) | the LLM tool service image: no pdfium, no config file, no credentials |
| [`keycloak/`](keycloak/README.md) | the development realm, imported by the `stack` profile |
| [`firecracker/`](firecracker/SPEC.md) | microVM image, the host agent (`fleet.sh`) and the autoscaler; a guest runs a copy of the service behind the host's load balancer |
| [`gpu/`](gpu/SPEC.md) | the model serving tier: vLLM per pool, compose and k8s manifests |

`firecracker` and `gpu` are how *this* project intends to run. Neither is a prerequisite
for embedding it: the service is an ordinary binary, and the model tier is any
OpenAI-compatible endpoint.
