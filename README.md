# visual-suite

Rust workspace for high-throughput visual document processing: per-page, content-aware
VLM workflows (Stage 1), document-level entity/relation graph extraction (Stage 2), and
traceable storage in a Postgres Data Vault 2.0 warehouse with vectors in Qdrant.

**Start with [`docs/SPEC.md`](docs/SPEC.md).** Every crate has its own `SPEC.md`.

## Layout

```
docs/SPEC.md              system spec: architecture, decisions, assumptions
crates/
  vs-core/                shared types (ids, blocks, page profile, provenance)
  vs-infra/               task bus (NATS), blob store (S3), config, telemetry
  vs-api/          [bin]  public HTTP API
  vs-ingest/              format detection, rasterization, text layer
  vs-classify/            page classification
  vs-dag/                 DAG model, step trait, executor, DAG list
  vs-router/              routing table + DAG selection
  vs-models/              model registry, vLLM client, model-backed steps
  vs-stitch/              page results -> coherent document
  vs-extract/             mentions, assertions, entity resolution
  vs-graph/               document graph model + builder, GraphStore trait
  vs-steps/               the production step catalogue
  vs-vault/               Data Vault 2.0 writer + SQL migrations; graph projection
  vs-vectors/             vector store trait + Qdrant (dense and sparse)
  vs-worker/       [bin]  worker roles: ingest | page | doc
  vs-adapt/        [bin]  workflow generation, scoring, selection (dev/stage)
deploy/
  firecracker/            microVM image + fleet spec
  gpu/                    model serving tier spec
config/                   example model registry, worker config, DAG list
```

## Status

All 16 crates compile together and the workspace is green: **199 unit tests pass, 16
integration tests are `#[ignore]`d** because they need Postgres, NATS, Garage or Qdrant
from `devenv up`. No `todo!()` or `unimplemented!()` remains anywhere in the tree.

Every crate's `SPEC.md` carries a `## Status` section with a checklist of what is left.
The cross-cutting gaps are:

- **No CI.** Nothing enforces the green build, so every status claim is hand-verified.
- **Telemetry is a stub** (`crates/vs-infra/src/telemetry.rs`): structured logs only, no
  OTLP export and no Prometheus endpoint on the workers. Only `vs-api` serves `/metrics`.
- **No auth.** `vs-api` takes the tenant from `X-VS-Tenant` and admin rights from
  `X-VS-Admin`, both of which any caller can set. Keep it on a trusted network.
- **Storage**: the graph and its history live in the Postgres Data Vault, vectors in
  Qdrant (`docs/SPEC.md`, decisions D6 and D9). HelixDB has been removed.
- `blocks.verify` appears in the `vs-dag` step catalogue but is not implemented.
- Figure and chart crops are never uploaded, so their `crop://…` URIs do not resolve.
- Stage 2 extraction quality and worker sizing have never been measured against a real
  dataset or a load test.

## Development environment

[devenv](https://devenv.sh) describes the system dependencies. Each service declares its
own in a `devenv.nix` next to its code; `devenv.yaml` imports them all:

| File | Provides |
|---|---|
| `devenv.nix` | Rust stable toolchain, cargo-nextest, `VS_CONFIG=config/worker.dev.toml` |
| `crates/vs-infra/devenv.nix` | NATS with JetStream (:4222), Garage S3 (:3900, bucket `visual-suite`, fixed dev key), `nats` CLI |
| `crates/vs-vault/devenv.nix` | PostgreSQL 17 (:5432, db/user/pass `visual_suite`/`vs`/`vs`), sqlx-cli, `DATABASE_URL`, `vs-migrate` |
| `crates/vs-ingest/devenv.nix` | pdfium (`PDFIUM_DYNAMIC_LIB_PATH`) |
| `crates/vs-vectors/devenv.nix` | Qdrant (gRPC :6334, REST :6333), `VS__QDRANT__URL` |
| `crates/vs-worker/devenv.nix` | `vs-worker-release` |
| `deploy/firecracker/devenv.nix` | firecracker, e2fsprogs; opt-in: `devenv --profile firecracker shell` |

```sh
devenv up -d          # postgres, nats, garage (layout, bucket, key and DB roles set up on start)
devenv shell          # or direnv
vs-migrate            # apply vs-vault migrations as the schema owner (API/workers only check)
devenv down
```

### Running the pipeline locally

The whole pipeline runs: upload → ingest (render, text layer, signals) → Stage 1 per page
(classify, route, execute the DAG) → Stage 2 per document (stitch, extract, graph, store).
Stage 1 and Stage 2 call models, so they need the endpoints in `--models` to be reachable;
with none configured the `ingest` role still runs on its own.

```sh
devenv up -d && devenv shell
cargo build -p vs-api -p vs-worker
cargo run -p vs-ingest --example sample_pdf -- sample.pdf
VS_API_ADDR=127.0.0.1:8080 ./target/debug/vs-api &
VS_ROLE=ingest ./target/debug/vs-worker &
VS_ROLE=page   ./target/debug/vs-worker &   # needs model endpoints in config/models.*.toml
VS_ROLE=doc    ./target/debug/vs-worker &   # needs model endpoints + the graph/vector store
curl -X PUT --upload-file sample.pdf --aws-sigv4 "aws:amz:$AWS_REGION:s3" \
  --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" "$AWS_ENDPOINT/visual-suite/inbox/t1/sample.pdf"
curl -X POST 127.0.0.1:8080/v1/documents -H 'x-vs-tenant: t1' -H 'content-type: application/json' \
  -d '{"language":"en","source_uri":"inbox/t1/sample.pdf"}'
curl 127.0.0.1:8080/v1/jobs/<job_id> -H 'x-vs-tenant: t1'
```

`X-VS-Tenant` is a development stand-in for auth (see `crates/vs-api/SPEC.md`).

### Tests

`cargo test` runs unit tests (PDF tests need `PDFIUM_DYNAMIC_LIB_PATH`, set by devenv).
Tests against Postgres, NATS and Garage are `#[ignore]`d; with `devenv up` running:

```sh
cargo test --workspace -- --include-ignored
```

## Build

```sh
cargo build --release
cargo test
```

The worker for Firecracker is a normal glibc build, shipped in the rootfs with its
shared libraries (pdfium cannot be loaded from a static musl binary):

```sh
cargo build --release -p vs-worker
```
