# Embedding the suite in another deployment

The default devenv shell builds the suite and **starts nothing**: no Postgres, no NATS, no
S3, no Keycloak. That is deliberate. Another project embedding this one wants the
binaries, not this repo's idea of where a database should live, so every endpoint is
configuration — and the local stack is a profile you can ignore:

```sh
devenv shell                      # toolchain, pdfium, sqlx-cli — no services
cargo build --release -p vs-suite
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

### There is no image here

This repo builds binaries, not containers: no Dockerfile for the service, and the only
image builds under `deploy/` are the model tier's. Wrapping `vs-suite` in your own image
is the intended path — it needs the binary, `libpdfium.so` next to it (the rasterizer
dlopens it), a CA bundle, and a config file. `deploy/firecracker/build-rootfs.sh` is a
worked example of exactly that packaging, for microVMs.

## Pointing it at your own services

Any key in that file is overridable by environment variable as
`VS__<SECTION>__<KEY>`, which is how to keep secrets out of the TOML:

| What | Config | Environment |
|---|---|---|
| Postgres | `[postgres] url`, `password`, `max_connections` | `VS__POSTGRES__URL`, `VS__POSTGRES__PASSWORD` |
| Task bus | `[bus] nats_url` | `VS__BUS__NATS_URL` |
| Blob store | `[blob] bucket`, `endpoint`, `region` | `VS__BLOB__BUCKET`, `VS__BLOB__ENDPOINT`; credentials come from the standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` (`AWS_ALLOW_HTTP=true` for a non-TLS endpoint) |
| Vectors | `[qdrant] url`, `collection_prefix` | `VS__QDRANT__URL` |
| Models | `[models] registry` → a models TOML | `VS__MODELS__REGISTRY` |
| Auth | `[auth] issuer`, `audience`, `client_id`, `admin_role`, `org_claim` | `VS__AUTH__ISSUER`, `VS__AUTH__AUDIENCE` (comma-separated: an override is always a scalar, so `audience` accepts a string as well as a list) … |
| Telemetry | `[telemetry] otlp_endpoint`, `metrics_addr` | `VS__TELEMETRY__OTLP_ENDPOINT` |

Three things are not negotiable, in the sense that the process will not come up without
them:

1. **An `[auth]` section**, with a reachable issuer. The `api` role fails at startup
   otherwise —
   there is no unauthenticated mode. Your realm needs an audience mapper, an organization
   claim and the admin role; see [`keycloak/README.md`](keycloak/README.md).
2. **Migrations applied, and the `vs_app` role existing.** `vs-suite migrate` runs as the
   schema owner; the API and the workers connect as the runtime role `vs_app`
   (insert-only on the vault, granted by migration 0003), which the migrations expect to
   exist already. Both the API and the workers *check* the schema version at startup and
   refuse to run against an unmigrated database.
3. **A bucket that exists**, with credentials that can read and write it. Nothing here
   creates it. Qdrant collections, by contrast, are created by the `doc` worker on first
   use.

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
- **Reading the stores** directly (the RAG service's job) is
  [`docs/STORES.md`](../docs/STORES.md).

## What is in this directory

| | |
|---|---|
| [`keycloak/`](keycloak/README.md) | the development realm, imported by the `stack` profile |
| [`firecracker/`](firecracker/SPEC.md) | worker microVM image and the fleet it is meant to become |
| [`gpu/`](gpu/SPEC.md) | the model serving tier: vLLM per pool, compose and k8s manifests |

`firecracker` and `gpu` are how *this* project intends to run. Neither is a prerequisite
for embedding it: the worker is an ordinary binary, and the model tier is any
OpenAI-compatible endpoint.
