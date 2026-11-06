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

Scaffold. Types, traits and the pure-logic parts (DAG validation, routing, scoring
objective, page rules) are implemented with unit tests. I/O-bound parts (model calls,
rasterization, stores, queue) have signatures and `todo!`/`NotImplemented` bodies,
described in each crate's spec.

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
| `crates/vs-worker/devenv.nix` | musl target, `vs-worker-static` |
| `deploy/firecracker/devenv.nix` | firecracker, e2fsprogs; opt-in: `devenv --profile firecracker shell` |

```sh
devenv up -d          # postgres, nats, garage (layout, bucket and key set up on start)
devenv shell          # or direnv
vs-migrate            # apply vs-vault migrations
devenv down
```

## Build

```sh
cargo build --release
cargo test
```

The worker for Firecracker is built as a static binary:

```sh
cargo build --release --target x86_64-unknown-linux-musl -p vs-worker
```
