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
docs/STORES.md      what a reader needs to query the two stores            contract
crates/
  vs-core/          shared types (ids, blocks, page profile, provenance)    README
  vs-dag/           DAG model, step trait, executor                         README
  vs-router/        routing table + DAG selection                           README
  vs-steps/         the production step catalogue                           README
  vs-graph/         document graph model + builder                          README
  vs-infra/         task bus (NATS), blob store (S3), config, telemetry     SPEC
  vs-api/     [bin] public HTTP API                                         SPEC
  vs-ingest/        format detection, rasterization, pre-processing         SPEC
  vs-classify/      page and document classification                        SPEC
  vs-models/        model registry, OpenAI client, model steps, parsers     SPEC
  vs-stitch/        page results -> document; intra-page layout             SPEC
  vs-extract/       mentions, assertions, entity resolution                 SPEC
  vs-vault/         Data Vault writer + migrations, incl. the graph layer   SPEC
  vs-vectors/       vector store trait + Qdrant (dense and sparse)          SPEC
  vs-worker/  [bin] worker roles: ingest | page | doc                       SPEC
  vs-adapt/   [bin] workflow generation, scoring, selection (dev/stage)     SPEC
deploy/
  firecracker/      microVM image + fleet                                   SPEC + README
  gpu/              model serving tier                                      SPEC + README
config/             example models, worker, DAG list, search space, LiteLLM
.github/            CI: clippy, the declared MSRV, and the integration tests
```

## Status

All 17 crates compile together and the workspace is green: **334 tests pass, none
ignored** — the 25 integration tests run against a live Postgres, Qdrant, NATS and Garage,
and one of them takes a document through the whole pipeline into both stores.
`cargo clippy --workspace --all-targets -- -D warnings` is clean. No `todo!()` or
`unimplemented!()` remains anywhere in the tree.

Five components are done and carry a README; the rest carry a `SPEC.md` whose `## Status`
section lists what is left. The cross-cutting gaps:

- **The models have never been real.** A document now goes upload → ingest → Stage 1 →
  Stage 2 → Postgres + Qdrant in one test (`crates/vs-worker/tests/pipeline.rs`), against
  real services and with the real handlers, so the handover between stages is covered.
  What stands in for the model tier is a fake server on a real socket, answering
  adaptively so grounding and verification are exercised rather than skipped. No VLM has
  ever parsed a page here, so extraction quality remains entirely unmeasured — see
  `vs-adapt`'s status.
- **No auth.** `vs-api` takes the tenant from `X-VS-Tenant` and admin rights from
  `X-VS-Admin`, both of which any caller can set. Keep it on a trusted network.
- **Telemetry is wired but unproven in a deployment.** One recorder per process, served
  on `telemetry.metrics_addr` for a worker and `/metrics` for `vs-api`, with the series
  each component's spec names; spans batch-export to `telemetry.otlp_endpoint` over
  OTLP/HTTP. Both are tested against real sockets, and neither has run against a real
  Prometheus or collector, so the dashboards and the sampling policy are still to come.
- **Nothing is measured.** `vs-adapt` has never been run on a real dataset, so
  content-aware routing (goal G2) and the pre-processing steps are unvalidated: whether
  deskewing or binarising helps any page class is exactly the open question. Stage 2
  extraction quality and worker sizing are likewise unmeasured.

Deliberately not built, each recorded with its reasoning in the owning component's docs:
dewarping (`vs-ingest` — a page-curl warp needs the page boundary, not ink statistics)
and automatic cross-page document-type smoothing (`vs-classify` — pages fan out with no
shared state, so it would serialise Stage 1). `page.orient` *is* built, with the
difference between its strong and weak halves stated in its own documentation: which axis
the text lines run along is measured, which end is up is a judgement with a margin.

## CI

`.github/workflows/ci.yml` runs three jobs on every push and pull request:

| Job | What it does | Why it is separate |
|---|---|---|
| `check` | `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo test --workspace` | the fast gate; clippy here has already caught a use of a 1.82 API under a declared MSRV of 1.80 |
| `msrv` | `cargo check` on exactly the toolchain `Cargo.toml` declares | so the MSRV is checked rather than asserted |
| `integration` | brings up Postgres, Qdrant, NATS and Garage, then `cargo test --workspace -- --include-ignored` | the first time these tests ever ran they found three real defects, two of them months old; this is also where the end-to-end pipeline test runs |

`.github/scripts/services.sh` starts the three services that a GitHub `services:` block
cannot (NATS needs an argument, Garage needs provisioning). It takes ports from the
environment, so the same script brings them up locally:

```sh
PREFIX=vs-ci QDRANT_GRPC=56334 NATS_PORT=54222 S3_PORT=53900 .github/scripts/services.sh
```

## Development environment

[devenv](https://devenv.sh) describes the system dependencies. Each service declares its
own in a `devenv.nix` next to its code; `devenv.yaml` imports them all:

| File | Provides |
|---|---|
| `devenv.nix` | Rust stable toolchain (the workspace needs ≥ 1.82), cargo-nextest, `VS_CONFIG=config/worker.dev.toml` |
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

`cargo test --workspace` runs the 309 tests that need nothing (PDF tests need
`PDFIUM_DYNAMIC_LIB_PATH`, set by devenv). The other 25 touch a real store and are
`#[ignore]`d; with `devenv up` running:

```sh
cargo test --workspace -- --include-ignored
```

**If devenv's ports are taken**, every service is also addressable by environment
variable, so the suite can be pointed at containers of your own on any ports. This is
worth knowing on a machine running more than one project — devenv wants 5432, 4222, 3900
and 6333, and will not start if something else holds them:

```sh
export DATABASE_URL=postgres://vs:vs@127.0.0.1:55432/visual_suite
export VS__POSTGRES__PASSWORD=vs_app          # the vs_app role must exist (see devenv.nix)
export VS__QDRANT__URL=http://127.0.0.1:56334 # gRPC, not the 6333 REST port
export VS__BUS__NATS_URL=nats://127.0.0.1:54222
export AWS_ENDPOINT=http://127.0.0.1:53900 AWS_REGION=garage AWS_ALLOW_HTTP=true
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
cargo test --workspace -- --include-ignored
```

Running these for the first time found three defects that no unit test could have caught,
two of them latent since Stage 2 was written: `write_stage2` never populated
`assertion_hk`, so every assertion insert violated a NOT NULL constraint and the
assertion write path had never worked; `sat_run`'s `hash_diff` included the write
timestamp, so a redelivered task versioned the satellite again; and `sat_image_text`'s
insert aliased its key column in a way `latest_differs` could not see. They are the
argument for running this set in CI.

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
