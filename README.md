# visual-suite

Rust workspace for high-throughput visual document processing: per-page, content-aware
VLM workflows (Stage 1), document-level entity/relation graph extraction (Stage 2), and
traceable storage in a Postgres Data Vault 2.0 warehouse with vectors in Qdrant.

It deploys as **one service** — `vs-suite`, which serves the HTTP API and runs all three
stages of the document pipeline in one process. Postgres, an S3-compatible store, Qdrant,
Keycloak and the model endpoints belong to the environment around it;
[`deploy/README.md`](deploy/README.md) is what another deployment needs to know.

A second, optional service — `vs-mcp` — advertises the same work as **LLM tools** over the
Model Context Protocol. It is a client of the API above: it holds no credentials, forwards
the caller's own token, and can be left out entirely (decision D16).

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
  vs-dag/           DAG model, step trait, page source, executor            README
  vs-router/        routing table + DAG selection                           README
  vs-steps/         the production step catalogue                           README
  vs-graph/         document graph model + builder                          README
  vs-infra/         blob store (S3, encrypted, swept), config, telemetry    SPEC
  vs-api/           public HTTP API: uploads, status, access, admin         SPEC
  vs-ingest/        format detection, rasterization, pre-processing         SPEC
  vs-classify/      page and document classification                        SPEC
  vs-models/        model registry, OpenAI client, model steps, parsers     SPEC
  vs-stitch/        page results -> document; intra-page layout             SPEC
  vs-extract/       mentions, assertions, entity resolution                 SPEC
  vs-vault/         Data Vault writer + migrations; the mutable ops tables  SPEC
  vs-vectors/       vector store trait + Qdrant (dense, sparse, fused)      SPEC
  vs-worker/        the three pipeline stages and their leases              SPEC
  vs-suite/   [bin] the service: the API, the stages and the sweeps          README
  vs-adapt/   [bin] workflow generation, scoring, selection, corpora        SPEC
  vs-mcp/     [bin] the LLM tool service: MCP tools over the HTTP API       SPEC
deploy/             embedding the suite in another deployment               README
  docker/           the service image (multi-stage, pinned pdfium)
  keycloak/         the development realm the suite verifies against         README
  firecracker/      microVM image, host agent, autoscaler                   SPEC + README
  gpu/              model serving tier, with a reserved interactive pool    SPEC + README
config/             example models, worker, DAG list, search space, LiteLLM
.github/            CI: clippy, the declared toolchain, and the integration tests
```

## Status

All 19 crates compile together and the workspace is green: **448 tests pass with nothing
running**, and **504 with the stack up** — the 56 extra touch a live Postgres, Qdrant,
Garage or Keycloak, one of them takes a document through the whole pipeline into both
stores, one asks the development realm for a token and verifies it, two take a generated
corpus through the entire adaptation chain, and four drive the MCP tools against the real
API router. `cargo clippy --workspace
--all-targets -- -D warnings` is clean. No `todo!()` or `unimplemented!()` anywhere in the
tree.

Five components carry a README because their design is fully built; the rest carry a
`SPEC.md` whose `## Status` section says what is left. **What is left is almost everywhere
a measurement, not a missing function**, and the distinction is the honest summary of this
repository:

### What is built and verified

- **The pipeline**, end to end: upload → ingest → Stage 1 per page → Stage 2 per document
  → Postgres and Qdrant, with provenance from every stored fact back to a page region, a
  DAG node, a model and a prompt.
- **Authentication and access.** Keycloak tokens verified against the realm's published
  keys; the tenant is the organization claim (D11). Inside an organization, access is the
  person's (D14): a document is private to its uploader by default and shared per document
  to a department, a workspace, the organization or named grantees. Sessions, tokens and
  people can be revoked before a token expires, driven by Keycloak's own back-channel
  logout. Erasure and retention are implemented for the one place a person's identifier
  lives.
- **Recovery.** A crashed copy's documents are leased, noticed and picked up by another
  copy's resume sweep, bounded so a document that kills a process cannot take down the
  fleet. Re-running resumes rather than repeats.
- **Retrieval, both halves.** Dense and sparse vectors are written *and* queried, fused
  inside Qdrant with reciprocal rank fusion, with a dimension guard and an aliased
  collection so a change of embedder is a migration rather than a rebuild.
- **The adaptation chain.** `vs-adapt dataset` writes a ground-truthed corpus and the five
  commands run as a chain over it, in a test, producing a DAG list the production router
  accepts. `vs-adapt classify-score` scores the page classifier itself — which found, on
  its first run, that a rules-only page was claiming 0.85 confidence in a document type
  nothing had determined.
- **The LLM tool service.** `vs-mcp` serves twelve tools over the Model Context Protocol,
  on stdio and over stateless HTTP, with protocol-version negotiation and the
  specification's own distinction between a protocol error and a tool failure the model can
  act on. It holds no credentials: the caller's token is forwarded and there is no
  service-account fallback, so a model can reach exactly what the person driving it can
  (D16). Nothing irreversible is advertised — no erasure, no token revocation, no routing
  administration — and a test keeps those names absent. Its live tests drive the real
  `vs_api::router` with real signed tokens, two people in one organization, and prove the
  refusal, the grant and the revocation through the tools.
- **Deployment.** One service image; a Firecracker host agent with per-VM egress
  allow-lists, snapshots and an autoscaler; a GPU tier with a replica set reserved for
  interactive uploads, because reserving slots inside the service never addressed the
  queue inside vLLM.

### What is not measured, and cannot be from here

- **The models have never been real.** A document goes upload → ingest → Stage 1 → Stage 2
  → Postgres + Qdrant in one test against real services and real handlers, with a fake
  OpenAI-compatible server on a real socket answering adaptively so that grounding and
  verification are exercised rather than skipped. **No VLM has ever parsed a page here**,
  so extraction quality is entirely unmeasured.
- **The corpus is synthetic.** `vs-adapt dataset` writes clean, born-digital, single-font
  PDFs labelled from the geometry that drew them. That makes the *machinery* provable and
  calibrates nothing: no skew, no scanner noise, no handwriting, no photographs, no
  entities to extract. Content-aware routing (goal G2) is still an untested claim.
- **No GPU has run this.** Every vLLM flag, memory fraction and HPA threshold in
  `deploy/gpu` is a reasoned starting point.
- **No fleet has run this.** `fleet.sh` is exercised by `--dry-run` and by hand; booting
  microVMs needs `/dev/kvm` and a TAP device, so the autoscaler's timings are untested and
  the snapshot warm start is unproven.
- **Telemetry has met no collector.** Metrics and spans are tested against sockets that
  record what they receive — which catches a setting that is read and ignored, and not a
  dashboard asking for the wrong series.
- **Nothing is sized.** Row counts, index growth, pages per copy before the model tier
  saturates, memory per concurrent document: all guesses, and all stated as guesses in the
  component that owns them.

Deliberately not built, each recorded with its reasoning where it belongs: dewarping
(`vs-ingest` — a page-curl warp needs the page boundary, not ink statistics), Stage 2 in
the adaptation search (`vs-extract` — it needs a second objective, not a second search
space), a sparse-embedding GPU pool (`deploy/gpu` — the lexical half is computed in
process and needs none), and per-tenant IDF statistics (`vs-vectors` — per-tenant
collections contradict the payload-partitioned layout, so the trade is offered as
`[qdrant].idf` instead).

## CI

`.github/workflows/ci.yml` runs three jobs on every push and pull request:

| Job | What it does | Why it is separate |
|---|---|---|
| `check` | `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo test --workspace` | the fast gate; clippy here has already caught a use of an API newer than the declared `rust-version` |
| `newest` | `cargo update`, then clippy and the tests on the newest stable | the policy is to track the newest toolchain and the newest dependencies, so this reports what upstream broke before a local `cargo update` does; informational, it does not block a merge |
| `integration` | brings up Postgres, Qdrant, Garage and Keycloak, then `cargo test --workspace -- --include-ignored` | the first time these tests ever ran they found three real defects, two of them months old; this is also where the end-to-end pipeline test runs |

`.github/scripts/services.sh` starts the three services that a GitHub `services:` block
cannot (Garage needs provisioning, Keycloak needs a realm imported). It takes ports from the
environment, so the same script brings them up locally:

```sh
PREFIX=vs-ci QDRANT_GRPC=56334 S3_PORT=53900 KEYCLOAK_PORT=58081 .github/scripts/services.sh
```

## Development environment

[devenv](https://devenv.sh) describes the system dependencies, in two halves. The
**default shell builds and unit-tests the suite and starts nothing** — no database, no
bus, no S3, no Keycloak. The **`stack` profile** adds every service the suite talks to. A
project that only wants the binaries never needs the second half; see
[`deploy/README.md`](deploy/README.md).

Each component declares both next to its own code: `devenv.nix` for what the build and
the client side need, `devenv-stack.nix` for the service itself.

| Default shell | Provides |
|---|---|
| `devenv.nix` | Rust stable toolchain (newest; `rust-version` follows it), cargo-nextest, `VS_CONFIG=config/worker.dev.toml` |
| `crates/vs-ingest/devenv.nix` | pdfium (`PDFIUM_DYNAMIC_LIB_PATH`) |
| `crates/vs-vault/devenv.nix` | sqlx-cli, `DATABASE_URL`, `vs-migrate` |
| `crates/vs-infra/devenv.nix` | the `AWS_*` values the blob store reads |
| `crates/vs-vectors/devenv.nix` | `VS__QDRANT__URL` |
| `crates/vs-worker/devenv.nix` | `vs-worker-release` |

| `stack` profile | Service |
|---|---|
| `devenv-stack.nix` | collects the five below; nothing else is in the profile |
| `crates/vs-vault/devenv-stack.nix` | PostgreSQL 17 (:5432, db/user/pass `visual_suite`/`vs`/`vs`), and the `vs`/`vs_app` roles |
| `crates/vs-infra/devenv-stack.nix` | Garage S3 (:3900, bucket `visual-suite`, fixed dev key) |
| `crates/vs-vectors/devenv-stack.nix` | Qdrant (gRPC :6334, REST :6333) |
| `crates/vs-models/devenv-stack.nix` | LiteLLM; `vs-litellm-up` / `vs-litellm-check` (scripts, not processes) |
| `deploy/keycloak/devenv.nix` | Keycloak (:8081) with the `koki` realm imported, and `vs-token` |

`deploy/firecracker/devenv.nix` is its own profile (firecracker, e2fsprogs; needs
`/dev/kvm`): `devenv --profile firecracker shell`.

```sh
devenv shell                     # build and unit-test the workspace; starts nothing
devenv --profile stack up -d     # postgres, garage, qdrant, keycloak
devenv --profile stack shell     # the same, plus vs-token and the model-tier scripts
vs-migrate                       # apply vs-vault migrations as the schema owner
devenv --profile stack down
```

### Running the pipeline locally

The whole pipeline runs: upload → ingest (render, text layer, signals) → Stage 1 per page
(classify, route, execute the DAG) → Stage 2 per document (stitch, extract, graph, store).
Stage 1 and Stage 2 call models, so they need reachable endpoints. `vs-litellm-up` with
`config/models.litellm.toml` is the easiest way to get them on one machine.

```sh
devenv --profile stack up -d && devenv --profile stack shell
cargo build -p vs-suite
cargo run -p vs-ingest --example sample_pdf -- sample.pdf
# One service: the API and the pipeline in one process. It needs model endpoints from
# config/models.*.toml at startup — every copy runs every stage — so bring up the model
# tier first (vs-litellm-up) or point [models] at one that exists.
VS_API_ADDR=127.0.0.1:8080 ./target/debug/vs-suite &
curl -X PUT --upload-file sample.pdf --aws-sigv4 "aws:amz:$AWS_REGION:s3" \
  --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" "$AWS_ENDPOINT/visual-suite/inbox/t1/sample.pdf"
# Every request carries a Keycloak access token; the tenant is its organization claim.
# `vs-token` asks the dev realm for one (organization t1, holding the admin role).
TOKEN=$(vs-token)
curl -X POST 127.0.0.1:8080/v1/documents -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"language":"en","source_uri":"inbox/t1/sample.pdf"}'
# `doc_type` is optional and trusted over anything inferred:
#   -d '{"language":"en","source_uri":"...","doc_type":"invoice"}'
curl 127.0.0.1:8080/v1/jobs/<job_id> -H "authorization: Bearer $TOKEN"

# A document is private to whoever uploaded it. `vs-token t1 other-person` is a second
# member of the same organization, who cannot read it until it is shared:
curl 127.0.0.1:8080/v1/documents/<document_hk> -H "authorization: Bearer $(vs-token)"          # 404 or the summary
curl -X PUT 127.0.0.1:8080/v1/documents/<document_hk>/access -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"visibility":"organization"}'
# And what this caller can read at all:
curl 127.0.0.1:8080/v1/documents -H "authorization: Bearer $TOKEN"
```

`vs-suite` refuses to start without a reachable realm, which is why Keycloak is part of
the `stack` profile rather than an extra: the profile imports
[`deploy/keycloak/realm-dev.json`](deploy/keycloak/README.md) on every start, so
`vs-token` works immediately and `vs-token t2` gives a second organization to check
isolation with. What each request is verified against is in
[`crates/vs-api/SPEC.md`](crates/vs-api/SPEC.md).

### Tests

`cargo test --workspace` runs the 448 tests that need nothing (PDF tests need
`PDFIUM_DYNAMIC_LIB_PATH`, set by devenv). The other 56 touch a real service, or render a
corpus, and are `#[ignore]`d; with the stack profile running:

```sh
devenv --profile stack up -d
cargo test --workspace -- --include-ignored
```

`vs-mcp`'s live tests sign tokens with the same fixtures and serve the whole realm —
discovery document and JWKS — on a socket, so `Auth::discover` runs the way a deployment
runs it rather than being bypassed.

The authentication tests sign their own tokens with the keypairs in
`crates/vs-api/fixtures/`. Those two private keys are committed deliberately — they are
throwaway test material, trusted by nothing and read by nothing outside the test module,
and committing them is what lets the tests verify real signatures instead of mocking the
verifier out. A secret scanner may still flag them (GitHub push protection among them);
`crates/vs-api/fixtures/README.md` says what they are and how to regenerate them.

**If devenv's ports are taken**, every service is also addressable by environment
variable, so the suite can be pointed at containers of your own on any ports. This is
worth knowing on a machine running more than one project — devenv wants 5432, 3900 and
6333, and moves or fails if something else holds them:

```sh
export DATABASE_URL=postgres://vs:vs@127.0.0.1:55432/visual_suite
export VS__POSTGRES__PASSWORD=vs_app          # the vs_app role must exist (see devenv.nix)
export VS__QDRANT__URL=http://127.0.0.1:56334 # gRPC, not the 6333 REST port
export AWS_ENDPOINT=http://127.0.0.1:53900 AWS_REGION=garage AWS_ALLOW_HTTP=true
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
cargo test --workspace -- --include-ignored
```

A Unix-socket `DATABASE_URL` works too (`postgres:///visual_suite?host=/run/user/1000/…`),
which is what devenv falls back to when 5432 is taken.

Running these for the first time found three defects that no unit test could have caught,
two of them latent since Stage 2 was written: `write_stage2` never populated
`assertion_hk`, so every assertion insert violated a NOT NULL constraint and the
assertion write path had never worked; `sat_run`'s `hash_diff` included the write
timestamp, so a redelivered task versioned the satellite again; and `sat_image_text`'s
insert aliased its key column in a way `latest_differs` could not see. They are the
argument for running this set in CI. Running them again after the access and lease work
found two more — a test that rebuilt a Unix-socket connection as TCP, and one asserting on
a binary name that has not existed since the suite became one service. Running the MCP
server's set for the first time found two more facts about the API itself, both of which a
fake would have hidden: a `source_uri` upload returns no document hash key unless
`skip_if_processed` makes the API read the object first, and a document's access row is
written by **ingest** rather than by accepting the job, so there is nothing to share until
a run exists.

### Measuring instead of guessing

The adaptation chain needs documents and labels, and generates both when you have none:

```sh
vs-adapt dataset --out corpus/                    # four page shapes, labelled exactly
vs-adapt generate --search-space config/search-space.example.json --out candidates.json
vs-adapt run   --manifest corpus/manifest.jsonl --candidates candidates.json --out runs/ --models models.toml
vs-adapt score --runs runs/runs.jsonl --out evals.jsonl
vs-adapt select --evals evals.jsonl --candidates candidates.json --version $(date +%F) \
                --out dag-list.json --report report.md
vs-adapt classify-score --manifest corpus/manifest.jsonl --out classify/ --report classifier.md
```

That corpus is synthetic and says so everywhere it appears: it proves the chain runs and
calibrates nothing. Pointing the same commands at real labelled documents is the one piece
of work no amount of code here replaces.

## Build

```sh
cargo build --release
cargo test
```

The Firecracker guest ships a normal glibc build with its shared libraries (pdfium cannot
be loaded from a static musl binary):

```sh
cargo build --release -p vs-suite
```
