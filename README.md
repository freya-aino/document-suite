# visual-suite

Rust workspace for high-throughput visual document processing: per-page, content-aware
VLM workflows (Stage 1), document-level entity/relation graph extraction (Stage 2), and
traceable storage in a Data Vault 2.0 warehouse and HelixDB.

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
  vs-graph/               document graph model + builder
  vs-vault/               Data Vault 2.0 writer + SQL migrations
  vs-helix/               HelixDB writer
  vs-worker/       [bin]  worker roles: ingest | page | doc
  vs-adapt/        [bin]  workflow generation, scoring, selection (dev/stage)
deploy/
  firecracker/            microVM image + fleet spec
  gpu/                    model serving tier spec
config/                   example model registry, worker config, DAG list
```

## Status

In progress. Implemented: shared types, DAG validation, routing, scoring objective, page
rules, stitching, rasterization (pdfium, images), task bus (NATS JetStream), blob store
(S3/Garage), vault writes for documents and pages, job state, the worker loop with the
ingest role, and document submission / job status in the API. Still stubbed (`todo!` /
`NotImplemented`, described in each crate's spec): model output parsers and most
model-backed steps, the page and doc worker roles, Stage 2 extraction and its vault
writes, the HelixDB writer, and most API endpoints.

## Development environment

[devenv](https://devenv.sh) describes the system dependencies. Each service declares its
own in a `devenv.nix` next to its code; `devenv.yaml` imports them all:

| File | Provides |
|---|---|
| `devenv.nix` | Rust stable toolchain, cargo-nextest, `VS_CONFIG=config/worker.dev.toml` |
| `crates/vs-infra/devenv.nix` | NATS with JetStream (:4222), Garage S3 (:3900, bucket `visual-suite`, fixed dev key), `nats` CLI |
| `crates/vs-vault/devenv.nix` | PostgreSQL 17 (:5432, db/user/pass `visual_suite`/`vs`/`vs`), sqlx-cli, `DATABASE_URL`, `vs-migrate` |
| `crates/vs-ingest/devenv.nix` | pdfium (`PDFIUM_DYNAMIC_LIB_PATH`) |
| `crates/vs-helix/devenv.nix` | `helix` CLI (no instance started yet) |
| `crates/vs-worker/devenv.nix` | `vs-worker-release` |
| `deploy/firecracker/devenv.nix` | firecracker, e2fsprogs; opt-in: `devenv --profile firecracker shell` |

```sh
devenv up -d          # postgres, nats, garage (layout, bucket, key and DB roles set up on start)
devenv shell          # or direnv
vs-migrate            # apply vs-vault migrations as the schema owner (API/workers only check)
devenv down
```

### Running the pipeline locally

Implemented so far: upload → ingest (render, text layer, signals) → vault + page tasks.
The `page` and `doc` workers do not start yet.

```sh
devenv up -d && devenv shell
cargo build -p vs-api -p vs-worker
cargo run -p vs-ingest --example sample_pdf -- sample.pdf
VS_API_ADDR=127.0.0.1:8080 ./target/debug/vs-api &
VS_ROLE=ingest ./target/debug/vs-worker &
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
