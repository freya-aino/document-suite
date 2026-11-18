# Embedding the suite in another deployment

The default devenv shell builds the suite and **starts nothing**: no Postgres, no NATS, no
S3, no Keycloak. That is deliberate. Another project embedding this one wants the
binaries, not this repo's idea of where a database should live, so every endpoint is
configuration — and the local stack is a profile you can ignore:

```sh
devenv shell                      # toolchain, pdfium, sqlx-cli — no services
cargo build --release -p vs-suite
docker build -f deploy/docker/Dockerfile -t document-suite:dev .   # or as an image
devenv --profile stack up -d      # only if you want this repo's services too
```

## The service

**One binary, one process, one service.** `vs-suite` serves the HTTP API and runs all
three worker roles together, so a deployment adds one unit and scales it by running more
copies. They coordinate through the task bus, so two copies divide the work the same way
separate processes would.

| | |
|---|---|
| Listens | `VS_API_ADDR`, default `0.0.0.0:8080` |
| Needs | Postgres, NATS, S3, Keycloak — and, for Stage 1 and Stage 2, model endpoints and Qdrant |
| Stop | SIGTERM: stops accepting requests and drains in-flight tasks within `--shutdown-grace` (60 s) |

Two other invocations of the same binary are not services: `vs-suite migrate` is a deploy
step (schema **owner** credentials, not the runtime role's), and `vs-suite render-child` is
what the ingest role re-executes per render — you never run it yourself.

### Splitting it, if you want to

`--roles` (`VS_ROLES`) narrows a copy, which is how the halves get scaled separately —
goal G8 as your choice rather than the default shape:

| Copy | Inbound port | Scales on |
|---|---|---|
| `vs-suite --roles api` | yes | request rate |
| `vs-suite --roles ingest` / `page` / `doc` | **none** — it pulls from NATS | JetStream lag for that role and lane |

A copy with no `api` role needs no `[auth]` section and no realm. A copy with it fails
startup without one. `worker.max_in_flight` applies per role, so a copy running all three
can have three times that many tasks in flight.

`vs-adapt` is the fourth binary and is never deployed: an offline tool for scoring
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
| `build` | `cargo build --release --locked -p vs-suite` on `rust:1.89-bookworm` |
| `runtime` | `debian:bookworm-slim`, the binary, the library, `ca-certificates`, and a minimal config |

Three things about it worth knowing before you change them:

- **pdfium is pinned to 8066, not "latest".** That is the version nixpkgs gives the dev
  shell and CI, so a page renders in the image exactly as it does in the tests. The
  release and its digest are build arguments (`PDFIUM_RELEASE`, `PDFIUM_SHA256`), so
  moving it is deliberate and verified rather than silent.
- **The builder is pinned to the declared MSRV** (`rust-version` in the workspace
  `Cargo.toml`, 1.89), so the image cannot be what quietly raises it, and building the
  image is one more check that the declaration is true.
- **The `HEALTHCHECK` reads `VS_ROLES`**, because only the `api` role has an endpoint to
  probe and a queue consumer reported unhealthy for having no HTTP server would be wrong.
  A worker-only copy must therefore set `VS_ROLES` rather than pass `--roles`, or run with
  `--no-healthcheck`.

It runs as uid 10001, writes to `/scratch`, and ships `/etc/vs/worker.toml` as the
smallest valid config: `[models]` pointing at `/etc/vs/models.toml`, and JSON logs.
`[bus]`, `[blob]`, `[postgres]` and `[qdrant]` are deliberately absent — there is no
sensible default for someone else's infrastructure, so the container stops with
`missing field <section>` until they are set, which is better than coming up pointed at
nothing. Mount your own file over `/etc/vs/worker.toml`, or set everything from the
environment; both work.

```sh
docker run --rm -p 8080:8080 \
  -v /path/to/models.toml:/etc/vs/models.toml:ro \
  -e VS__BUS__NATS_URL=nats://nats:4222 \
  -e VS__BLOB__BUCKET=visual-suite -e VS__BLOB__ENDPOINT=https://s3.internal \
  -e VS__POSTGRES__URL=postgres://vs_app@pg:5432/visual_suite -e VS__POSTGRES__PASSWORD=… \
  -e VS__QDRANT__URL=http://qdrant:6334 \
  -e VS__AUTH__ISSUER=https://sso.example.org/realms/koki -e VS__AUTH__AUDIENCE=vs-api \
  -e AWS_ACCESS_KEY_ID=… -e AWS_SECRET_ACCESS_KEY=… -e AWS_REGION=… \
  document-suite:dev
```

`docker run … document-suite:dev migrate --database-url …` runs the deploy step from the
same image, as the schema owner.

There is **no compose file and no chart** here: how the service is wired to your Postgres,
your NATS and your realm is your deployment's business, and this repo guessing at it would
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
| `--roles` | `VS_ROLES` | `api,ingest,page,doc` — any comma-separated subset |
| `--config` | `VS_CONFIG` | `/etc/vs/worker.toml`; the file must exist, even if empty |
| `--addr` | `VS_API_ADDR` | `0.0.0.0:8080`; ignored without the `api` role |
| `--shutdown-grace` | `VS_SHUTDOWN_GRACE` | `60` seconds to drain on SIGTERM |
| `migrate --database-url` | `DATABASE_URL` | —; the deploy step, as the schema **owner** |

Ports: `8080` inbound for the `api` role, plus `telemetry.metrics_addr` if set. The worker
roles open none.

### The config file

**Required sections**: `[bus]`, `[blob]`, `[postgres]`, `[qdrant]`, `[models]`. A missing
one is a startup error (`Error: missing field 'qdrant'`). `[qdrant]` is required even for a
copy that never reaches Stage 2, on purpose: a misconfigured deployment should fail at
startup, not at the first document that gets that far.

**Optional sections**: `[worker]`, `[api]`, `[telemetry]`, `[classify]`, `[stage2]`, and
`[auth]` — which the `api` role then requires at runtime.

| Key | Default | |
|---|---|---|
| `bus.nats_url` | **required** | |
| `blob.bucket` | **required** | nothing creates it |
| `blob.endpoint` | none | an `http://` endpoint enables plain HTTP by itself |
| `blob.region` | none | |
| `postgres.url` | **required** | without the password |
| `postgres.password` | none | from the environment only |
| `postgres.max_connections` | `16` | per **process**, shared by every role in it |
| `qdrant.url` | `http://127.0.0.1:6334` | the **gRPC** port; 6333 is REST and will not work |
| `qdrant.api_key_env` | none | the *name* of the variable holding the key |
| `qdrant.collection_prefix` | `vs` | lets one Qdrant host several deployments |
| `qdrant.batch_size` | `256` | points per upsert |
| `models.registry` | **required** | path to the model-registry TOML |
| `auth.issuer` | **required** | matched against `iss` exactly; the base for OIDC discovery |
| `auth.audience` | **required** | a list, or one comma-separated string |
| `auth.client_id` | none | whose `resource_access.<id>.roles` are read as well as realm roles |
| `auth.org_claim` | `organization` | **becomes the tenant**; a dotted path reads a nested claim |
| `auth.department_claim` | `department` | recorded on the job; not enforced |
| `auth.workspaces_claim` | `workspaces` | recorded on the job; not enforced |
| `auth.admin_role` | `vs-admin` | the DAG-list routes and `/metrics` |
| `auth.jwks_refresh_s` | `300` | how long a fetched JWKS is reused |
| `auth.leeway_s` | `30` | clock skew allowed on `exp` and `nbf` |
| `api.max_upload_bytes` | `2147483648` | 413 above it |
| `api.interactive_max_pending` | `1000` | 429 for interactive above this lane backlog |
| `api.scratch_dir` | system temp | upload spool while hashing |
| `worker.max_in_flight` | `32` | **per role**: all three in one copy is three times this |
| `worker.interactive_share` | `0.3` | slots reserved for the interactive lane |
| `worker.render_dpi` | `200` | |
| `worker.max_failed_page_fraction` | `0.05` | above it the document fails |
| `worker.scratch_dir` | system temp | where ingest spools originals |
| `worker.render_processes` | available CPUs | render children per ingest role |
| `worker.page_render_timeout_s` | `60` | a slower page fails its document permanently |
| `worker.render_mem_limit_mb` | none | address-space cap per render child |
| `telemetry.otlp_endpoint` | none | OTLP over **HTTP** (4318, not gRPC's 4317); bare origin, `/v1/traces` is appended |
| `telemetry.metrics_addr` | none | **unauthenticated** — bind an internal interface, never `0.0.0.0`; leave unset when the `api` role serves `/metrics` |
| `telemetry.json_logs` | `false` | |
| `stage2.extraction_model` | `extract-llm` | ids from `models.registry`, not URLs |
| `stage2.verify_model` | `extract-llm` | may equal the extraction model |
| `stage2.embedding_model` | `embed-text` | |
| `stage2.judge_model` | none | unset = thresholds decide corpus resolution alone |
| `stage2.doc_classifier_model` | none | unset = the document type is the weighted page vote |
| `stage2.classifier_model` | none | unset = rules-only page classification |
| `stage2.window_chars` | `12000` | characters per extraction window |
| `stage2.type_match_threshold` | `0.85` | cosine, for mapping raw types onto the ontology |
| `stage2.link_threshold` | `0.92` | links an entity without asking the judge |
| `stage2.judge_band` | `[0.80, 0.92]` | the band the judge decides in — an array, so **not** settable by environment |
| `stage2.embed_batch_size` | `0` | 0 = the registry's per-model batch size |
| `stage2.dag_list_refresh_s` | `60` | how long a role trusts its cached active DAG list |
| `classify.*` | see `ClassifyConfig::default` | `born_digital_min_chars`, `born_digital_min_coverage`, `min_image_area_ratio`, `trust_ocr_layer`, `skip_vision_for_text_only_digital`, `flag_threshold`, `rules_only_confidence`, `thumbnail_long_side_px`, `blank_ink_coverage` |

### Overriding from the environment

`VS__<SECTION>__<KEY>`, upper case, `__` between levels — which is how secrets stay out of
the file. Three things to know:

- **Scalars only.** An override parses to an integer, float, bool or string and never to
  an array, so `auth.audience` accepts a comma-separated string for exactly this reason,
  and `stage2.judge_band` cannot be set this way at all.
- **A required section needs at least one key from somewhere.** An override creates its
  parent table, so `VS__QDRANT__URL=…` satisfies `[qdrant]` on its own; set nothing for a
  required section and the process stops with `missing field`.
- **An almost-empty file works.** A zero-byte TOML plus the variables below is a complete
  configuration — useful when the whole deployment is environment-driven.

Read outside that scheme:

| | |
|---|---|
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT`, `AWS_ALLOW_HTTP` | S3 credentials and endpoint, through `AmazonS3Builder::from_env()`. `blob.endpoint` and `blob.region` are applied afterwards, so they win |
| `PDFIUM_DYNAMIC_LIB_PATH` | the directory holding `libpdfium.so`; the `ingest` role cannot render without it |
| whatever `qdrant.api_key_env` and each model's `api_key_env` name | the keys themselves |
| `RUST_LOG` | tracing filter, e.g. `info,vs_=debug` |
| `SSL_CERT_FILE`, `TMPDIR` | CA bundle, scratch location |
| `DATABASE_URL` | the `migrate` subcommand only |

### The minimum for the whole service

```sh
VS_CONFIG=/etc/vs/worker.toml            # may be empty
VS__BUS__NATS_URL=nats://nats:4222
VS__BLOB__BUCKET=visual-suite
VS__BLOB__ENDPOINT=https://s3.internal
VS__POSTGRES__URL=postgres://vs_app@pg:5432/visual_suite
VS__POSTGRES__PASSWORD=…
VS__QDRANT__URL=http://qdrant:6334
VS__MODELS__REGISTRY=/etc/vs/models.toml
VS__AUTH__ISSUER=https://sso.example.org/realms/koki
VS__AUTH__AUDIENCE=vs-api
AWS_ACCESS_KEY_ID=…  AWS_SECRET_ACCESS_KEY=…  AWS_REGION=…
PDFIUM_DYNAMIC_LIB_PATH=/usr/lib
```

Drop the `VS__AUTH__*` pair only for a copy running no `api` role; with it, startup fails
as `the api role needs an [auth] section in …`.

### What must exist before it starts

1. **An `[auth]` section**, with a reachable issuer. The `api` role fails at startup
   otherwise — there is no unauthenticated mode. Your realm needs an audience mapper, an
   organization claim and the admin role, including on the service account your Prometheus
   scrape uses; see [`keycloak/README.md`](keycloak/README.md).
2. **Migrations applied, and the `vs_app` role existing.** `vs-suite migrate` runs as the
   schema owner; the service connects as the runtime role `vs_app` (insert-only on the
   vault, granted by migration 0003), which the migrations expect to exist already. The
   service *checks* the schema version at startup and refuses an unmigrated database.
3. **A bucket that exists**, with credentials that can read and write it. Nothing here
   creates it. Qdrant collections, by contrast, the `doc` role creates on first use.

## Operating it

- **Health**: `GET /healthz` (process up) and `GET /readyz` (Postgres and the blob store
  reachable) are the only unauthenticated routes, for load balancers. A copy running no
  `api` role serves neither: it has no HTTP server, so liveness is the process itself.
- **Metrics**: `GET /metrics` needs the admin role — scrape it as a service account. A
  copy without the `api` role has no HTTP server, and serves its metrics on
  `telemetry.metrics_addr` instead; that port has no authentication, so bind it to an
  internal interface and never `0.0.0.0`. Setting both gives you both.
- **Scaling**: more copies of the service. Within a copy, the roles share one Postgres
  pool and one NATS connection, so `postgres.max_connections` is a per-process budget.
- **Tenancy**: the tenant is the organization claim of the caller's token. It is hashed
  into every document and entity key, prefixes every blob key, and partitions the Qdrant
  collections, so **changing what the claim contains re-keys the data** — pick it once.
- **Personal data**: telemetry has none — no span field, log line or metric label
  identifies a person, which is asserted by a test rather than a convention
  (`docs/SPEC.md` §5.6). What does hold it is `ops.jobs.spec` (`owner`: the caller's
  subject, department and workspaces) and the task payloads on the bus, where the DLQ
  keeps failed tasks for 14 days. The Data Vault holds none of it, which is what makes an
  erasure request answerable at all: the vault is insert-only and never forgets.
  **Nothing implements retention or erasure for those `ops` rows** — if your deployment is
  subject to GDPR, that is yours to add, and the shape it should take is a decision about
  the identifier (keep the subject, keep a hash, or keep a mapping elsewhere).
- **Reading the stores** directly (the RAG service's job) is
  [`docs/STORES.md`](../docs/STORES.md).

## What is in this directory

| | |
|---|---|
| [`docker/`](docker/Dockerfile) | the service image: multi-stage build, pinned pdfium, minimal baked config |
| [`keycloak/`](keycloak/README.md) | the development realm, imported by the `stack` profile |
| [`firecracker/`](firecracker/SPEC.md) | worker microVM image and the fleet it is meant to become |
| [`gpu/`](gpu/SPEC.md) | the model serving tier: vLLM per pool, compose and k8s manifests |

`firecracker` and `gpu` are how *this* project intends to run. Neither is a prerequisite
for embedding it: the worker is an ordinary binary, and the model tier is any
OpenAI-compatible endpoint.
