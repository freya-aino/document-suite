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

## Build

```sh
cargo build --release
cargo test
```

The worker for Firecracker is built as a static binary:

```sh
cargo build --release --target x86_64-unknown-linux-musl -p vs-worker
```
