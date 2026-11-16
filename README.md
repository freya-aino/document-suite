# visual-suite

Rust workspace for high-throughput visual document processing: per-page, content-aware
VLM workflows (Stage 1), document-level entity/relation graph extraction (Stage 2), and
traceable storage in a Postgres Data Vault 2.0 warehouse with vectors in Qdrant.

**Start with [`docs/SPEC.md`](docs/SPEC.md).**

Every component documents itself next to its code. A component whose design is fully
built carries a `README.md` describing what exists; one with work outstanding carries a
`SPEC.md` with a `## Status` checklist of what is left. The component map in
`docs/SPEC.md` links to whichever each has.

## Layout

```
docs/SPEC.md        system spec: architecture, decisions, assumptions
                                                                            docs
crates/
  vs-core/          shared types (ids, blocks, page profile, provenance)    README
  vs-dag/           DAG model, step trait, executor                         README
  vs-router/        routing table + DAG selection                           README
  vs-steps/         the production step catalogue                           README
  vs-graph/         document graph model + builder, GraphStore trait        README
  vs-infra/         task bus (NATS), blob store (S3), config, telemetry     SPEC
  vs-api/     [bin] public HTTP API                                         SPEC
  vs-ingest/        format detection, rasterization, pre-processing         SPEC
  vs-classify/      page and document classification                        SPEC
  vs-models/        model registry, OpenAI client, model steps, parsers     SPEC
  vs-stitch/        page results -> document; intra-page layout             SPEC
  vs-extract/       mentions, assertions, entity resolution                 SPEC
  vs-vault/         Data Vault writer + migrations; the graph store         SPEC
  vs-vectors/       vector store trait + Qdrant (dense and sparse)          SPEC
  vs-worker/  [bin] worker roles: ingest | page | doc                       SPEC
  vs-adapt/   [bin] workflow generation, scoring, selection (dev/stage)     SPEC
deploy/
  firecracker/      microVM image + fleet                                   SPEC + README
  gpu/              model serving tier                                      SPEC + README
config/             example models, worker, DAG list, search space, LiteLLM
```

## Status

All 17 crates compile together and the workspace is green: **290 unit tests pass, 17
integration tests are `#[ignore]`d** because they need Postgres, NATS, Garage or Qdrant
from `devenv up`. No `todo!()` or `unimplemented!()` remains anywhere in the tree.

Five components are done and carry a README; the rest carry a `SPEC.md` whose `## Status`
section lists what is left — 84 items in total. The cross-cutting gaps:

- **No CI.** Nothing enforces the green build, so every claim here and in those
  checklists is verified by hand.
- **Nothing has run end to end.** Each stage is tested in isolation against fakes; no run
  goes upload → ingest → Stage 1 → Stage 2 → Postgres + Qdrant. The storage layer in
  particular has never been exercised against a live Postgres or Qdrant.
- **No auth.** `vs-api` takes the tenant from `X-VS-Tenant` and admin rights from
  `X-VS-Admin`, both of which any caller can set. Keep it on a trusted network.
- **Telemetry is a stub** (`crates/vs-infra/src/telemetry.rs`): structured logs only, no
  OTLP export and no Prometheus endpoint on the workers. Only `vs-api` serves `/metrics`,
  so every per-crate metric these docs specify is unobservable in a worker.
- **Nothing is measured.** `vs-adapt` has never been run on a real dataset, so
  content-aware routing (goal G2) and the pre-processing steps are unvalidated: whether
  deskewing or binarising helps any page class is exactly the open question. Stage 2
  extraction quality and worker sizing are likewise unmeasured.
- Figure and chart crops are never uploaded, so their `crop://…` URIs do not resolve.
- `blocks.verify` appears in the `vs-dag` step catalogue and is not implemented.

Deliberately not built, each recorded with its reasoning in the owning component's docs:
orientation correction and dewarping (`vs-ingest` — neither is decidable from pixel
statistics), and automatic cross-page document-type smoothing (`vs-classify` — pages fan
out with no shared state, so it would serialise Stage 1).

## Development environment

[devenv](https://devenv.sh) describes the system dependencies. Each service declares its
own in a `devenv.nix` next to its code; `devenv.yaml` imports them all:

| File | Provides |
|---|---|
| `devenv.nix` | Rust stable toolchain, cargo-nextest, `VS_CONFIG=config/worker.dev.toml` |
| `crates/vs-infra/devenv.nix` | NATS with JetStream (:4222), Garage S3 (:3900, bucket `visual-suite`, fixed dev key), `nats` CLI |
| `crates/vs-vault/devenv.nix` | PostgreSQL 17 (:5432, db/user/pass `visual_suite`/`vs`/`vs`), sqlx-cli, `DATABASE_URL`, `vs-migrate` |
| `crates/vs-ingest/devenv.nix` | pdfium (`PDFIUM_DYNAMIC_LIB_PATH`) |
| `crates/vs-models/devenv.nix` | LiteLLM; `vs-litellm-up` / `vs-litellm-check` (not started by `devenv up`) |
| `crates/vs-vectors/devenv.nix` | Qdrant (gRPC :6334, REST :6333), `VS__QDRANT__URL` |
| `crates/vs-worker/devenv.nix` | `vs-worker-release` |
| `deploy/firecracker/devenv.nix` | firecracker, e2fsprogs; opt-in: `devenv --profile firecracker shell` |

```sh
devenv up -d          # postgres, nats, garage, qdrant (layout, bucket, key and DB roles set up on start)
devenv shell          # or direnv
vs-migrate            # apply vs-vault migrations as the schema owner (API/workers only check)
devenv down
```

### Running the pipeline locally

The whole pipeline runs: upload → ingest (render, text layer, signals) → Stage 1 per page
(classify, route, execute the DAG) → Stage 2 per document (stitch, extract, graph, store).
Stage 1 and Stage 2 call models, so they need reachable endpoints. `vs-litellm-up` with
`config/models.litellm.toml` is the easiest way to get them on one machine; with no model
tier at all the `ingest` role still runs on its own.

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
# `doc_type` is optional and trusted over anything inferred:
#   -d '{"language":"en","source_uri":"...","doc_type":"invoice"}'
curl 127.0.0.1:8080/v1/jobs/<job_id> -H 'x-vs-tenant: t1'
```

`X-VS-Tenant` is a development stand-in for auth — see
[`crates/vs-api/SPEC.md`](crates/vs-api/SPEC.md).

### Tests

`cargo test --workspace` runs the 290 unit tests (PDF tests need
`PDFIUM_DYNAMIC_LIB_PATH`, set by devenv). The 17 tests that need Postgres, NATS, Garage
or Qdrant are `#[ignore]`d; with `devenv up` running:

```sh
cargo test --workspace -- --include-ignored
```

Those 17 are the ones that matter most and the ones least often run: they are the only
tests that touch a real store, so the Postgres and Qdrant write paths are unverified
until they pass. Run them before trusting anything in the storage layer.

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
