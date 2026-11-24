# Visual Suite — System Specification

Version: 0.7 · Status: implemented; one document verified end to end against live stores with a fake model tier; authenticated against Keycloak, scoped per person, and recoverable after a crash · Date: 2026-11-23

This is the top-level spec: purpose, architecture, the component map, the cross-cutting
contracts, and the decisions. Each component has its own `SPEC.md` next to its code
(see [Component map](#4-component-map)), and **that file is where the component's detail,
its assumptions and its remaining work live**. A component whose design is fully built
carries a `README.md` describing what exists; one with work outstanding carries a
`SPEC.md` with a `## Status` checklist. The Spec column of the component map says which. If a component
spec and this file disagree on a cross-cutting contract, this file wins; on component
detail, the component spec wins.

---

## 1. Purpose

Extract as much traceable information as possible from heterogeneous documents
(born-digital PDFs, scans, photos, handwriting, embedded images, tables, formulas,
charts) and load it into:

- a **relational Data Vault 2.0** warehouse in Postgres — the system of record, the audit
  trail, and the document graph the existing RAG service traverses, and
- **Qdrant** for the dense and sparse vectors that same service searches.

Every stored fact must be traceable back to a page region, the workflow (DAG) that
produced it, and the exact model and prompt version used.

## 2. Goals and non-goals

### Goals (v1)

| ID | Goal |
|----|------|
| G1 | Throughput first: page-level parallelism, GPU batching, skip work that a page does not need. |
| G2 | **Content-aware routing**: each page is classified and runs the best-scoring DAG for its class. |
| G3 | **Offline workflow adaptation**: in dev/stage, generate and score candidate DAGs against datasets; promote the winners as a versioned *DAG list* to prod. |
| G4 | Two-stage processing: **Stage 1** per page (OCR, layout, images, tables…) → stitching → **Stage 2** per document (entities, relations, graph). |
| G5 | Document-level graph, not flat extraction. Scientific papers set the bar for relation extraction quality. |
| G6 | Open-schema extraction with automatic type normalisation. |
| G7 | Full provenance: block bbox + character span, run, DAG, step, model, prompt. |
| G8 | ~~Frontend/API and backend scale independently~~ — **withdrawn** by D13. There is nothing between them to scale across: a copy takes whole documents. Batch first, and interactive work still gets reserved capacity, now within a copy rather than in its own queue lane. |
| G9 | Everything in Rust. Workers run in Firecracker microVMs. |

### Non-goals (v1)

- Automatic language detection (caller sends a hint; detection is post-v1).
- Real-time / streaming SLAs.
- Agentic document navigation at ingest (candidate for the RAG service later).
- Human review UI (the schema supports review status; no UI).
- Training or fine-tuning models.

## 3. Architecture

```
            ┌───────────────────────────────────────────────────────────────┐
            │  vs-suite  — one binary, one process, one deployable service  │
            │                                                               │
 clients ──▶│  api    : upload, status, DAG-list admin                      │
 (your      │     │                                                         │
 service-   │     └─▶ pipeline, per accepted document:                      │
 api)       │          ingest : detect, rasterize, text layer               │
            │          pages  : classify → route → run DAG   (concurrent)   │
            │          doc    : stitch → extract → graph                    │
            └───────────────────┬───────────────────────────────────────────┘
                                │ HTTP (OpenAI-compatible)
                                ▼
                       ┌──────────────────────┐        ┌──────────────────┐
                       │ GPU model tier (vLLM)│        │ Keycloak (OIDC)  │
                       │ doc-parse VLMs,      │        │ the realm every  │
                       │ general VLM, LLM,    │        │ request is       │
                       │ embedders, classifier│        │ verified against │
                       └──────────────────────┘        └──────────────────┘

       ┌──────────────────────┐   ┌───────────────┐   ┌──────────────────────┐
       │ Postgres             │   │ Object store  │   │ Qdrant               │
       │  ops schema (jobs)   │   │ (S3-compat.)  │   │ dense + sparse       │
       │  vault schema (DV2.0)│   │ originals,    │   │ vectors              │
       │   = history + graph  │   │ page images,  │   └──────────┬───────────┘
       │  registry (DAG lists)│   │ page results  │              │
       └──────────┬───────────┘   └───────────────┘              │
                  └──────────── RAG service reads ───────────────┘
```

**A second service, optional** (decision D16): `vs-mcp` advertises the suite's work as LLM
tools over the Model Context Protocol. It is a *client* of the API above — it links none of
the suite's internals and holds no credentials, forwarding the caller's own token — so it
can be deployed, scaled, restarted or left out without the suite noticing.

```
LLM client ──MCP (stdio or HTTP)──▶ vs-mcp ──the caller's token──▶ vs-suite's api
```

**No task bus** (decision D13). There was one — NATS JetStream, one message per page,
drained by a fleet of `page` workers. The per-page DAG is executed in-process either way,
so the queue's only unique contribution was spreading a single document's pages across
machines, and it cost a service, three deployable roles and a redelivery protocol. The
stages are now function calls inside the process that accepted the upload; pages still run
concurrently, bounded by `worker.max_in_flight` and by the model tier they share with
every other copy.

**One service** (decision D12): a deployment adds one unit and scales it by running more
copies. Each copy takes whole documents, so scaling is per document rather than per page
— the trade decision D13 made.

That is also what brought the Firecracker tier back. It was specified for queue consumers
and marked superseded when the bus went, because a microVM takes no inbound connections
and there was no way to push a document to one. A copy of the service *serves HTTP*, and a
guest's address is reachable from its host across its own TAP device, so a load balancer on
the host hands it uploads like any other copy — see
[`deploy/firecracker`](../deploy/firecracker/SPEC.md), which now has a host agent and an
autoscaler.

**Key decision D1:** Firecracker has no GPU passthrough, so microVMs run CPU work only
(I/O, rendering, classification rules, orchestration, parsing model output, writes).
Models are served by a separate GPU tier over HTTP. See `deploy/firecracker/SPEC.md`
and `deploy/gpu/SPEC.md`.

### 3.1 Production flow

```
upload(lang hint, priority, [profile], [dag_list pin])
  └─▶ ingest (per document)
        detect format → page count → native text layer → rasterize pages → blob store
        └─▶ fan out N page tasks
              Stage 1 (per page, parallel)
                classify page → PageProfile
                route(PageProfile, policy, DAG list) → DAG
                execute DAG → PageResult (blocks + content + provenance)
                write page/block hubs to vault; PageResult to blob store
                last page done → enqueue doc task
              Stage 2 (per document)
                stitch (cross-page tables, split paragraphs, captions, reading order)
                build structure graph
                extract mentions + assertions (binary and n-ary) with evidence
                resolve entities (in-document, then cross-corpus canonicalisation)
                embed blocks / figures / entities
                write vault (graph, entities, assertions, evidence) + Qdrant (vectors)
```

### 3.2 Adaptation flow (dev / stage only)

Datasets → candidate DAGs → scored per page class → the winners promoted as a versioned
DAG list. The five commands and their inputs and outputs are specified once, in
[`vs-adapt/SPEC.md`](../crates/vs-adapt/SPEC.md) "Commands".

## 4. Component map

| Crate / folder | Kind | Responsibility | Docs |
|---|---|---|---|
| `vs-core` | lib | Shared types: IDs, hash keys, geometry, blocks, page profile, provenance, jobs | [README](../crates/vs-core/README.md) |
| `vs-infra` | lib | Blob store (with encryption and retention), config, telemetry | [SPEC](../crates/vs-infra/SPEC.md) |
| `vs-api` | lib | Public HTTP API: uploads, batches, status, access and sharing, DAG-list admin | [SPEC](../crates/vs-api/SPEC.md) |
| `vs-ingest` | lib | Format detection, rasterization, native text layer, CPU steps | [SPEC](../crates/vs-ingest/SPEC.md) |
| `vs-classify` | lib | Page classification → `PageProfile` | [SPEC](../crates/vs-classify/SPEC.md) |
| `vs-dag` | lib | DAG model, step trait, validation, executor | [README](../crates/vs-dag/README.md) |
| `vs-router` | lib | DAG list bundle (DAGs + routing table), DAG selection per page | [README](../crates/vs-router/README.md) |
| `vs-models` | lib | Model registry, OpenAI-compatible client, model-backed steps, output parsers | [SPEC](../crates/vs-models/SPEC.md) |
| `vs-steps` | lib | Production `StepRegistry` shared by worker, API and vs-adapt | [README](../crates/vs-steps/README.md) |
| `vs-stitch` | lib | Page results → one coherent document | [SPEC](../crates/vs-stitch/SPEC.md) |
| `vs-extract` | lib | Mentions, assertions, entity resolution | [SPEC](../crates/vs-extract/SPEC.md) |
| `vs-graph` | lib | Document graph model and builder | [README](../crates/vs-graph/README.md) |
| `vs-vault` | lib | Data Vault 2.0 writer + migrations; the graph layer, corpus entity resolution, and the `ops` tables that must be mutable (access, leases, revocations) | [SPEC](../crates/vs-vault/SPEC.md) |
| `vs-vectors` | lib | Vector store trait + Qdrant implementation (dense, sparse, and the two fused) | [SPEC](../crates/vs-vectors/SPEC.md) |
| `vs-worker` | lib | The three pipeline stages, their leases, and the document-type pass before the fan-out | [SPEC](../crates/vs-worker/SPEC.md) |
| `vs-suite` | **bin** | **The service**: the API, all three stages, and the sweeps that keep them honest, in one process | [README](../crates/vs-suite/README.md) |
| `vs-adapt` | bin | Workflow generation, scoring, selection; a generated corpus and the classifier's own score | [SPEC](../crates/vs-adapt/SPEC.md) |
| `vs-mcp` | **bin** | **The LLM tool service**: the suite's work as Model Context Protocol tools, over its HTTP API | [SPEC](../crates/vs-mcp/SPEC.md) |
| `deploy/` | ops | Embedding the suite in another deployment: binaries, the configuration surface, what must exist first | [README](../deploy/README.md) |
| `deploy/docker` | ops | The service image: multi-stage build, pinned pdfium, minimal baked config; and a small separate image for `vs-mcp` | [Dockerfile](../deploy/docker/Dockerfile) |
| `deploy/keycloak` | ops | The development realm `vs-api` verifies against | [README](../deploy/keycloak/README.md) |
| `deploy/firecracker` | ops | microVM image, the host agent and the autoscaler | [SPEC](../deploy/firecracker/SPEC.md) |
| `deploy/gpu` | ops | Model serving tier | [SPEC](../deploy/gpu/SPEC.md) |
| `config/` | config | Example models, worker and DAG-list files | — |

Dependency direction (no cycles):
`vs-core` ← `vs-infra`, `vs-dag` ← `vs-router`, `vs-classify`, `vs-ingest`, `vs-models`
← `vs-stitch` ← `vs-extract` ← `vs-graph` ← `vs-vectors` ← `vs-vault` ← `vs-worker`,
`vs-api`, `vs-adapt` ← `vs-suite`, which depends on both `vs-api` and `vs-worker`.

`vs-mcp` sits outside that chain: it depends on `vs-infra` for telemetry and on nothing
else of the suite's — it reaches `vs-suite` over HTTP. The one exception is its
`dev-dependencies`, where `vs-api` and `vs-vault` appear so that `tests/live.rs` can serve
the real router against real Postgres. Both `vs-suite` and `vs-mcp` are *services*
(`vs-adapt` is an offline CLI).

`vs-graph` owns the `DocGraph` both stores are written from, so it sits below every store
crate and depends on none of them.

## 5. Cross-cutting concerns

These are the contracts every component must honour. How each component honours them is
in its own `SPEC.md`.

### 5.1 Identifiers and provenance

- **Hash keys** follow Data Vault conventions: `SHA-256` over normalised business keys
  joined with `|`, hex-encoded. Defined once in `vs-core::ids`; the formula for each key
  type is in [`vs-core/README.md`](../crates/vs-core/README.md).
- **Run** = one execution of the pipeline for one document under one DAG-list version.
  `run_id` is a UUIDv7.
- **Provenance** is attached to every block, mention and assertion:
  `run_id, dag_id@version, node_id, step_kind, model_id@revision, prompt_id@hash,
  params_hash, started_at, duration_ms, confidence`.
- **Evidence** (A9): assertions point to `(block_hk, char_start, char_end)` spans and, for
  visual evidence, `(page_hk, bbox)`. Every stored fact traces back to a page region.

### 5.2 Scaling and throughput

Pages are the unit of parallelism *within* a document, and **skipping work is the main
lever** — the routing table encodes which pages may skip which steps. The suite is one
service with no queue (D12, D13), so it scales by running more copies, each taking whole
documents: request rate and document throughput scale together, and a single document is
as fast as one copy can make it.
[`deploy/gpu`](../deploy/gpu/SPEC.md) on queue depth and batch fill per model pool;
[`vs-router`](../crates/vs-router/README.md) decides what is skipped.

### 5.3 Priority

`priority` is still `interactive` or `batch` on every upload (A20), and still means the
same thing: interactive work must not queue behind a large batch. It is enforced in two
places, because one of them was never enough:

- **In the process**, by reserving a share of its page slots
  (`worker.interactive_share`) for interactive documents —
  [`vs-worker`](../crates/vs-worker/SPEC.md). This stops a batch from filling a copy.
- **At the GPU**, by sending interactive calls to replicas the batch cannot reach
  (`interactive_endpoints` in the model registry, D15). Reserving slots in the service
  does nothing about the batch's requests already queued in a vLLM instance: a thousand
  batch pages and one interactive page queue together, and the page waits behind them.

Admission and back-pressure: [`vs-api`](../crates/vs-api/SPEC.md).

### 5.4 Reliability

**Every write is idempotent** — through hash keys and `ON CONFLICT DO NOTHING`. That was
required when delivery was at-least-once, and it is what makes recovery cheap now that
nothing redelivers: a document that is run again resumes rather than repeats (rendered
pages are skipped when their signals are present, and no vault row is written twice).

**A crashed copy's documents are picked up.** Each job is leased by the process working on
it and renewed while it works; a lease that stops being renewed is what a crash looks like
from outside, and every copy sweeps for lapsed leases and claims them with
`FOR UPDATE SKIP LOCKED`. `worker.max_resumes` bounds how often one document may be picked
up, so a document that kills the process reading it cannot take the fleet down one copy at
a time — past the bound its job is failed with a reason the caller can read. This is what
replaced redelivery when the bus went (D13), and it is deliberately less than a queue:
nothing spreads one document's pages across machines, and a resumed document is processed
by one copy like every other. The sweep: [`vs-suite`](../crates/vs-suite/README.md); the
lease: [`vs-worker`](../crates/vs-worker/SPEC.md).

In-process retries for a transient page failure and the per-page give-up path:
[`vs-worker`](../crates/vs-worker/SPEC.md). Partial-document threshold:
[`vs-worker`](../crates/vs-worker/SPEC.md). Per-step timeouts:
[`vs-dag`](../crates/vs-dag/README.md). Insert-only history:
[`vs-vault`](../crates/vs-vault/SPEC.md).

### 5.5 Observability

Standard span fields on every trace: `job_id`, `run_id`, `document_hk`, `page_no`,
`node_id`, plus `tenant` on an HTTP request — and **no identifier of a person**, which is
a hard rule, not a default (§5.6). Traces are sampled parent-based at `telemetry.sample_ratio` (1.0 by default, so nothing is
dropped until a deployment chooses to), which keeps a trace whole rather than exporting it
in pieces. Exporters and the Prometheus
endpoint: [`vs-infra`](../crates/vs-infra/SPEC.md). Each component's metric names are in
its own spec, and both halves carry one name for the process — a `service` label on every
series, the same string as the traces' `service.name` — so a counter and a trace from the
same process are joinable: `vs-suite` for a copy running the whole service, and
`vs-suite-<roles>` (`vs-suite-page`, `vs-suite-api-ingest`) for one running part of it.

### 5.6 Security and personal data

**Telemetry carries no user-specific identifier.** Not a metric label, not a span field,
not a log line. Spans are exported to a collector and logs are shipped, each with its own
retention and its own readers, so a Keycloak `sub` in them is personal data in a store
that was never designed to hold any, cannot be found again, and cannot be erased —
GDPR data minimisation (Art. 5(1)(c)) and storage limitation (Art. 5(1)(e)). The
*organization* is present, deliberately: it identifies a tenant rather than a person, and
without it an operator cannot tell whose work a trace belongs to. Which **person** made a
request is answered by Keycloak's own audit log, the system that is allowed to know.

Enforced rather than asked for: `Principal::subject` and `Owner::subject` are redacted in
their `Debug` implementations, so a principal or a job spec reaching a log line cannot
carry one, and `vs-api`'s `telemetry_carries_no_user_specific_identifier` captures
everything a subscriber sees during two real requests and fails if a subject or a token id
appears.

Where personal data *does* live, and nowhere else:

| Store | What | Retention |
|---|---|---|
| `ops.jobs.owner_subject` and `ops.jobs.spec.owner` | the caller's subject, department and workspaces, from the verified token — what per-person access control is evaluated against | scrubbed from finished jobs after `api.job_retention_days` (90), and immediately on erasure |
| `ops.document_access` | the uploader of each document, and how wide it is open | cleared on erasure; the document stays and becomes the organization's |
| `ops.document_grants` | who a document was shared with, and by whom | deleted on erasure, either side |
| ~~Task payloads on the bus~~ | — | **gone with the bus** (D13): the `JobSpec` no longer leaves the process, so there is no queue or dead-letter copy of it to age out |
| Keycloak | the identity itself | the realm's own policy |

The **Data Vault holds none of it**: no satellite stores the owner, which matters because
the vault is insert-only by design (D5) and an erasure request cannot be served by a store
that never forgets. Deleting a person's data therefore means `ops` rows and nothing else,
and `DELETE /v1/owners/{subject}` is that operation — allowed to an administrator and to
the person themselves, because an erasure request is the data subject's to make. Decision
D14 is the reason the rows are where they are.

Tenancy is part of every hash key, and the tenant never comes from a request body — nor
from a header. **The tenant is the organization**, taken from a verified Keycloak token
(D11), which makes it the isolation boundary: every document and entity key hashes it,
every blob key is prefixed by it, and the Qdrant collections are partitioned on it.

**Inside** an organization, access is the person's (D14). A document is private to its
uploader by default (`api.default_visibility`), widened per document to a department, a
workspace or the whole organization, with named grants as exceptions; a job is the asker's,
plus whoever can read the document it produced. One function decides
(`vs_vault::access::Reader::may_read`), the SQL behind the listing only narrows, and a
`403` says so rather than a `404` pretending otherwise — a document's hash key is derived
from its content, so a caller who can name one already has the bytes.

A verified token is also no longer good until its `exp` whatever has happened to it:
sessions, tokens and people can be revoked, written by Keycloak's own back-channel logout
or by an administrator, with `api.revocation_refresh_s` as the stated window. Auth, the
checks each request is held to, and the access rule in full:
[`vs-api`](../crates/vs-api/SPEC.md); what a realm must provide and what an embedding
deployment must configure: [`deploy/`](../deploy/README.md). Network isolation and the
per-microVM egress allow-lists:
[`deploy/firecracker`](../deploy/firecracker/SPEC.md). Secrets from the environment, and
encryption at rest: [`vs-infra`](../crates/vs-infra/SPEC.md).

## 6. Decisions

| ID | Decision | Reason |
|----|----------|--------|
| D1 | CPU work in Firecracker, models on a separate GPU tier over HTTP | Firecracker has no GPU passthrough |
| D2 | One binary with roles | One rootfs image, simpler fleet |
| D3 | Model access through an OpenAI-compatible API | Works with vLLM (and SGLang, LiteLLM, hosted APIs) without code changes |
| D4 | Workflows are static DAGs, chosen per page from a versioned DAG list | Reproducible; optimisation happens offline in `vs-adapt` |
| D5 | Data Vault is insert-only; reprocessing adds satellites with a new run | Natural DV2.0 history; old results stay traceable |
| D6 | The Postgres Data Vault holds both the full history **and** the current graph projection | One system of record; the vault already stores richer evidence (region as well as span) than a separate graph store did |
| D7 | Structured model output via guided decoding (JSON schema) | Parseable output, fewer hallucinated formats |
| D8 | Large intermediate artefacts (page images, page results) live in the object store; queues carry references only | Small messages, no shared FS in Firecracker |
| D9 | Vectors live in a dedicated vector store (Qdrant) behind a `VectorStore` trait, keyed by tenant and run | The vector store stays swappable; the graph stays in SQL next to its provenance |
| D10 | Authentication is Keycloak (OIDC) access tokens verified in `vs-api`, against the realm's published keys | The SSO that already holds the identity model; verifying a signature locally keeps Keycloak off the path of every request, unlike introspection |
| D11 | **The tenant is the organization**, from a verified claim | The isolation boundary has to be the thing that `TenantId` already is: key material in every hash key, blob prefix and vector partition. Organization also makes one corpus per company, so entity resolution and duplicate detection work across colleagues — which a per-person tenant would break, since the same file uploaded twice would be two documents with two keys. Person, department and workspace are therefore read scoping *within* a tenant, not key material |
| D12 | **One service**: `vs-suite` serves the API and runs the pipeline in one process | A deployment embedding the suite should add one service, not learn its internal shape. `vs-api` and `vs-worker` are libraries |
| D13 | **No task bus.** The stages are function calls; an accepted upload is processed by the process that accepted it | The per-page DAG always ran in-process, so the queue's only unique contribution was spreading one document's pages across machines — paid for with a service, three deployable roles, redelivery, dead-lettering, lanes and de-duplication ids. Page work is dominated by waiting on the shared model tier, which one process can saturate. The cost, accepted knowingly: a large document is no longer spread over the fleet. Recovery after a crash came back as a **lease and a resume sweep** rather than as a queue (§5.4). Withdraws A6 and G8 |
| D14 | **Access control lives in `ops`, not in the vault.** Ownership, visibility, grants, job leases, token revocations and the one personal identifier are all mutable rows beside the jobs | The raw vault is insert-only by design (D5) and at the database level (migration 0003 grants the runtime role neither UPDATE nor DELETE), so a share could not be withdrawn there and an erasure request could not be served there. Putting ownership columns on satellites was the obvious reading of "scope the documents" and it is the wrong one: it would make every re-share a new satellite row and every erasure impossible. `ops` is also where the only other personal identifier already was, so there is one surface to erase rather than two. The cost: a read goes to `ops` as well as the vault, which is one indexed lookup on a primary key |
| D15 | **A replica set reserved for interactive work**, declared per model as `interactive_endpoints` | Reserving page slots inside the service stops a batch from filling a *copy* and does nothing about the batch's requests already queued in a vLLM instance: a thousand batch pages and one interactive page queue together at the GPU, and the page waits behind them. Short of a scheduler inside the server, separate replicas are the only fix. A deployment with one pool declares nothing and is unaffected |
| D16 | **The MCP server is a separate service that holds no credentials**: `vs-mcp` is its own binary, speaks to `vs-suite` over its public HTTP API, and forwards the caller's own bearer token with no service-account fallback | Two decisions in one, and the second is the load-bearing one. *Separate*, against D12's instinct, because an MCP server's trust surface is prompt-shaped — a document whose text asks the model to share it — and that belongs outside the process holding the database credentials; it also scales, fails and deploys separately, and a deployment wanting no LLM tools runs one fewer binary. *No credentials*, because a service account would make every tool call run as the server, so every model user would read everything the server's identity could reach — D14's per-person access control removed by a configuration default. The consequence is the property that keeps the surface small: this service can do nothing a person with the same token could not do over HTTP, and no tool takes an organization, a subject or an "act as" |

**D14 is worth reading as a correction.** `vs-api`'s own status used to say that scoping
reads "needs ownership columns in the vault and a migration". That was the obvious design
and it was wrong for a reason the vault's own rules make plain, which is why the decision
is recorded with the alternative it rejects rather than only with what was built.

**D6/D9 replaced the original HelixDB decisions.** HelixDB held the current graph
projection and the vectors. It has been removed in favour of Postgres (graph) and Qdrant
(vectors), so the suite has one system of record instead of two stores that could
disagree about what was extracted, and the vector store sits behind a trait so the next
change of mind is cheap. Remaining work is tracked in the `## Status` checklists of
[`vs-vault`](../crates/vs-vault/SPEC.md) and [`vs-vectors`](../crates/vs-vectors/SPEC.md).

## 7. Assumptions

Only the two genuinely cross-cutting assumptions stay here. The other eighteen moved into
the spec of the component that rests on them, under "Assumptions this component rests on".

| ID | Assumption |
|----|-----------|
| A9 | Traceability at bbox + character-span level (see §5.1). |
| A11 | No human review loop in v1; confidence and review status are stored but unused. |

Where the rest went: A1, A2, A7 → [`vs-models`](../crates/vs-models/SPEC.md) and
[`vs-infra`](../crates/vs-infra/SPEC.md) · A3, A4 →
[`deploy/gpu`](../deploy/gpu/SPEC.md), [`deploy/firecracker`](../deploy/firecracker/SPEC.md)
· A5, A10, A12 → [`vs-vault`](../crates/vs-vault/SPEC.md) · A6 → withdrawn with the task bus (D13) · A8 → withdrawn with HelixDB (its licensing question is moot) · A13, A15, A16 →
[`vs-adapt`](../crates/vs-adapt/SPEC.md) · A14 →
[`vs-classify`](../crates/vs-classify/SPEC.md) · A17 →
[`vs-extract`](../crates/vs-extract/SPEC.md) · A18 →
[`vs-ingest`](../crates/vs-ingest/SPEC.md) · A19, A20 →
[`vs-api`](../crates/vs-api/SPEC.md).

## 8. Open questions

1. GPU hardware and the expected pages/day range, to size the pools →
   [`deploy/gpu`](../deploy/gpu/SPEC.md).
2. Which document types matter most after scientific papers, to seed the adaptation
   datasets → [`vs-adapt`](../crates/vs-adapt/SPEC.md).
3. Ground-truth availability, especially relation-annotated scientific papers →
   [`vs-adapt`](../crates/vs-adapt/SPEC.md). A *synthetic* corpus exists now
   (`vs-adapt dataset`) and exercises the chain; it calibrates nothing, so this question
   is unchanged for everything that matters.

Resolved, with the answers in the components that act on them:

- **Retention for originals and page images** (was question 4). `[blob.retention]`:
  derived objects are kept for days-to-months by class and swept, and **originals are kept
  forever** unless a deployment says otherwise — deleting a tenant's input is not this
  software's decision to make. [`vs-infra`](../crates/vs-infra/SPEC.md).
- **Read scoping inside an organization** (was question 5). All three, as a per-document
  `visibility` — private, department, workspace, organization — with named grants as
  exceptions, defaulting to private. A shared workspace is a snapshot taken at upload, so a
  document shared with a workspace stays shared with it after the uploader leaves.
  [`vs-api`](../crates/vs-api/SPEC.md), decision D14.

Resolved earlier: the authentication design. It was "mTLS or a JWT from your service-api — to be
decided"; it is now Keycloak tokens verified in `vs-api`, with the organization as the
tenant (D10, D11). Also resolved: what the RAG service needs from the graph store. It now queries Postgres and
Qdrant directly, and the contract it reads is [`docs/STORES.md`](STORES.md) — key
formulas, all eight retrieval patterns as SQL, the Qdrant collection and payload layout,
and the responsibilities that moved to the caller when the compiled queries went away.

## 9. Research basis

Moved into the components that act on it: the parsing-VLM landscape and the output-family
table → [`vs-models`](../crates/vs-models/SPEC.md); the scoring benchmarks
(OmniDocBench, PureDocBench, ParseBench, Dr. DocBench) →
[`vs-adapt`](../crates/vs-adapt/SPEC.md); graph-construction research (KG4VD,
MMGraphRAG) → [`vs-graph`](../crates/vs-graph/README.md) and
[`vs-extract`](../crates/vs-extract/SPEC.md).

Agentic document navigation (SCoPE VLM, EACL 2026) stays a v1 non-goal: it belongs at
query time, in the RAG service, not at ingest.
