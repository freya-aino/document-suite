# Embedding the suite in another deployment

The default devenv shell builds the suite and **starts nothing**: no Postgres, no NATS, no
S3, no Keycloak. That is deliberate. Another project embedding this one wants the
binaries, not this repo's idea of where a database should live, so every endpoint is
configuration — and the local stack is a profile you can ignore:

```sh
devenv shell                      # toolchain, pdfium, sqlx-cli — no services
cargo build --release -p vs-api -p vs-worker
devenv --profile stack up -d      # only if you want this repo's services too
```

## The binaries

| Binary | Role | Listens | Needs |
|---|---|---|---|
| `vs-api` | ingress: upload, status, DAG-list admin | `VS_API_ADDR`, default `0.0.0.0:8080` | Postgres, NATS, S3, Keycloak |
| `vs-worker --role ingest` | rasterize, text layer, page signals | — (also `telemetry.metrics_addr`) | Postgres, NATS, S3 |
| `vs-worker --role page` | Stage 1 per page | — | the above + model endpoints |
| `vs-worker --role doc` | Stage 2 per document | — | the above + Qdrant |
| `vs-api migrate` | schema migrations; a deploy step, not a service | — | Postgres, as the schema **owner** |
| `vs-adapt` | offline workflow scoring (dev/stage) | — | nothing at rest |

All of them read one TOML file — `--config`, or `VS_CONFIG`. Start from
[`config/worker.example.toml`](../config/worker.example.toml); it documents every key.

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

1. **An `[auth]` section**, with a reachable issuer. `vs-api` fails at startup otherwise —
   there is no unauthenticated mode. Your realm needs an audience mapper, an organization
   claim and the admin role; see [`keycloak/README.md`](keycloak/README.md).
2. **Migrations applied, and the `vs_app` role existing.** `vs-api migrate` runs as the
   schema owner; the API and the workers connect as the runtime role `vs_app`
   (insert-only on the vault, granted by migration 0003), which the migrations expect to
   exist already. Both the API and the workers *check* the schema version at startup and
   refuse to run against an unmigrated database.
3. **A bucket that exists**, with credentials that can read and write it. Nothing here
   creates it. Qdrant collections, by contrast, are created by the `doc` worker on first
   use.

## Operating it

- **Health**: `GET /healthz` (process up) and `GET /readyz` (Postgres and the blob store
  reachable) are the only unauthenticated routes, for load balancers.
- **Metrics**: `GET /metrics` on `vs-api` needs the admin role — scrape it as a service
  account. A worker serves its own on `telemetry.metrics_addr`, which has no
  authentication, so bind it to an internal interface and never `0.0.0.0`.
- **Scaling**: `vs-api` on request rate; the workers on JetStream lag per role and lane.
  The roles are independent processes and scale separately.
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
