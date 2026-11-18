# Visual Suite — System Specification

Version: 0.6 · Status: implemented; one document verified end to end against live stores, with a fake model tier; authenticated against Keycloak · Date: 2026-11-17

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
| G8 | Frontend/API and backend scale independently. Batch first; small batches get a faster lane. |
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
 clients ──▶│  api    : upload, status, DAG-list admin  ──jobs──┐           │
 (your      │                                                   │           │
 service-   │  ingest : detect, rasterize, text layer   ◀──┐    │           │
 api)       │  page   : classify → route → run DAG      ◀──┤    │           │
            │  doc    : stitch → extract → graph        ◀──┤    │           │
            └───────────────────┬──────────────────────────┼────┼───────────┘
                                │                          │    ▼
                                │              ┌───────────┴─────────────────┐
                                │              │ Task bus (NATS JetStream)   │
                                │              │ lanes: interactive | batch  │
                                │              │ subjects: ingest|page|doc   │
                                │              └─────────────────────────────┘
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

**One service** (decision D12): the four roles are concurrency, not packaging. A
deployment adds one service and scales it by running more copies; `--roles` narrows a
copy to part of the work when the halves should scale separately (goal G8), which is then
a choice that deployment makes rather than one the suite makes for it. The roles still
hand work to each other over the bus, so two copies of the whole service share the queue
exactly as a split deployment would. In the Firecracker CPU tier a microVM runs worker
roles only, because it takes no inbound connections.

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
| `vs-infra` | lib | Task bus, blob store, config, telemetry | [SPEC](../crates/vs-infra/SPEC.md) |
| `vs-api` | lib | Public HTTP API: uploads, batches, status, DAG-list admin | [SPEC](../crates/vs-api/SPEC.md) |
| `vs-ingest` | lib | Format detection, rasterization, native text layer, CPU steps | [SPEC](../crates/vs-ingest/SPEC.md) |
| `vs-classify` | lib | Page classification → `PageProfile` | [SPEC](../crates/vs-classify/SPEC.md) |
| `vs-dag` | lib | DAG model, step trait, validation, executor | [README](../crates/vs-dag/README.md) |
| `vs-router` | lib | DAG list bundle (DAGs + routing table), DAG selection per page | [README](../crates/vs-router/README.md) |
| `vs-models` | lib | Model registry, OpenAI-compatible client, model-backed steps, output parsers | [SPEC](../crates/vs-models/SPEC.md) |
| `vs-steps` | lib | Production `StepRegistry` shared by worker, API and vs-adapt | [README](../crates/vs-steps/README.md) |
| `vs-stitch` | lib | Page results → one coherent document | [SPEC](../crates/vs-stitch/SPEC.md) |
| `vs-extract` | lib | Mentions, assertions, entity resolution | [SPEC](../crates/vs-extract/SPEC.md) |
| `vs-graph` | lib | Document graph model and builder | [README](../crates/vs-graph/README.md) |
| `vs-vault` | lib | Data Vault 2.0 writer + migrations; graph projection and corpus entity resolution | [SPEC](../crates/vs-vault/SPEC.md) |
| `vs-vectors` | lib | Vector store trait + Qdrant implementation (dense and sparse) | [SPEC](../crates/vs-vectors/SPEC.md) |
| `vs-worker` | lib | The worker roles `ingest`, `page`, `doc` | [SPEC](../crates/vs-worker/SPEC.md) |
| `vs-suite` | **bin** | **The service**: the API and all three roles in one process | [README](../crates/vs-suite/README.md) |
| `vs-adapt` | bin | Workflow generation, scoring, selection | [SPEC](../crates/vs-adapt/SPEC.md) |
| `deploy/` | ops | Embedding the suite in another deployment: binaries, the configuration surface, what must exist first | [README](../deploy/README.md) |
| `deploy/docker` | ops | The service image: multi-stage build, pinned pdfium, minimal baked config | [Dockerfile](../deploy/docker/Dockerfile) |
| `deploy/keycloak` | ops | The development realm `vs-api` verifies against | [README](../deploy/keycloak/README.md) |
| `deploy/firecracker` | ops | microVM image and fleet | [SPEC](../deploy/firecracker/SPEC.md) |
| `deploy/gpu` | ops | Model serving tier | [SPEC](../deploy/gpu/SPEC.md) |
| `config/` | config | Example models, worker and DAG-list files | — |

Dependency direction (no cycles):
`vs-core` ← `vs-infra`, `vs-dag` ← `vs-router`, `vs-classify`, `vs-ingest`, `vs-models`
← `vs-stitch` ← `vs-extract` ← `vs-graph` ← `vs-vectors` ← `vs-vault` ← `vs-worker`,
`vs-api`, `vs-adapt` ← `vs-suite`, which depends on both `vs-api` and `vs-worker` and is
the only binary that is a *service* (`vs-adapt` is an offline CLI).

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

Page tasks are the unit of parallelism, and **skipping work is the main lever** — the
routing table encodes which pages may skip which steps. The suite is one service
(D12), so the default is to scale copies of it; what each part scales on, for a
deployment that splits them with `--roles`:
the [`api`](../crates/vs-api/SPEC.md) role on request rate;
the [worker roles](../crates/vs-worker/SPEC.md) and
[`deploy/firecracker`](../deploy/firecracker/SPEC.md) on JetStream lag per role and lane;
[`deploy/gpu`](../deploy/gpu/SPEC.md) on queue depth and batch fill per model pool;
[`vs-router`](../crates/vs-router/README.md) decides what is skipped.

### 5.3 Priority lanes

Two lanes, `interactive` and `batch`: the same code path over different streams and
consumers. Stream topology and lane fairness:
[`vs-infra`](../crates/vs-infra/SPEC.md). Admission and back-pressure:
[`vs-api`](../crates/vs-api/SPEC.md).

### 5.4 Reliability

Delivery is at-least-once, so **every write must be idempotent** — through hash keys and
`ON CONFLICT DO NOTHING`. Retries, dead-lettering and the max-deliveries give-up path:
[`vs-infra`](../crates/vs-infra/SPEC.md). Partial-document threshold:
[`vs-worker`](../crates/vs-worker/SPEC.md). Per-step timeouts:
[`vs-dag`](../crates/vs-dag/README.md). Insert-only history:
[`vs-vault`](../crates/vs-vault/SPEC.md).

### 5.5 Observability

Standard span fields on every trace: `job_id`, `run_id`, `document_hk`, `page_no`,
`node_id`, plus `tenant` on an HTTP request — and **no identifier of a person**, which is
a hard rule, not a default (§5.6). Exporters and the Prometheus
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
| `ops.jobs.spec` (JSONB) | `owner`: the caller's subject, department and workspaces, from the verified token — needed for per-person access control | **none implemented** |
| Task payloads on the bus | the same `JobSpec` | work-queue: deleted on ack. The DLQ keeps failed tasks 14 days |
| Keycloak | the identity itself | the realm's own policy |

The **Data Vault holds none of it**: no satellite stores the owner, which matters because
the vault is insert-only by design (D5) and an erasure request cannot be served by a store
that never forgets. Deleting a person's data therefore means `ops` rows and the DLQ, not
the vault. Retention and an erasure path for those are **not implemented** — see
[`deploy/README.md`](../deploy/README.md).

Tenancy is part of every hash key, and the tenant never comes from a request body — nor
from a header. **The tenant is the organization**, taken from a verified Keycloak token
(D11), which makes it the isolation boundary: every document and entity key hashes it,
every blob key is prefixed by it, and the Qdrant collections are partitioned on it. The
rest of the caller's identity — person, department, workspaces — is recorded on the job
(`JobSpec::owner`) but is **not yet enforced**: inside one organization, every member can
read every document. Auth, the checks each request is held to, and that gap:
[`vs-api`](../crates/vs-api/SPEC.md); what a realm must provide and what an embedding
deployment must configure: [`deploy/`](../deploy/README.md). Network isolation and the no-inbound-ports rule:
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
| D12 | **One service**: `vs-suite` serves the API and runs all three worker roles in one process | A deployment embedding the suite should add one service, not learn its internal shape. The roles already communicate over the bus, so one process or four changes nothing about the work; `--roles` keeps G8 available without making it the default. `vs-api` and `vs-worker` are libraries |

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
· A5, A10, A12 → [`vs-vault`](../crates/vs-vault/SPEC.md) · A6 →
[`vs-infra`](../crates/vs-infra/SPEC.md) · A8 → withdrawn with HelixDB (its licensing question is moot) · A13, A15, A16 →
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
   [`vs-adapt`](../crates/vs-adapt/SPEC.md).
4. Retention rules for originals and page images →
   [`vs-infra`](../crates/vs-infra/SPEC.md).

5. Whether read scoping inside an organization is per person, per department or per
   workspace, and what a shared workspace means for a document — the question that
   decides the ownership columns the vault would need →
   [`vs-api`](../crates/vs-api/SPEC.md).

Resolved: the authentication design. It was "mTLS or a JWT from your service-api — to be
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
