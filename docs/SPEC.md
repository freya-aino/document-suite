# Visual Suite — System Specification

Version: 0.1 (draft) · Status: scaffold · Date: 2026-11-06

This is the top-level spec. Each component has its own `SPEC.md` next to its code
(see [Component map](#4-component-map)). If a component spec and this file disagree,
this file wins until the component spec is updated.

---

## 1. Purpose

Extract as much traceable information as possible from heterogeneous documents
(born-digital PDFs, scans, photos, handwriting, embedded images, tables, formulas,
charts) and load it into:

- a **relational Data Vault 2.0** warehouse (the system of record and audit trail), and
- **HelixDB** (graph + vector store) for traversal and retrieval by the existing RAG service.

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
            ┌──────────────┐        ┌──────────────────────────────────────────┐
 clients ──▶│  vs-api      │──jobs─▶│  Task bus (NATS JetStream)                │
 (your     │  (stateless, │        │  lanes: interactive | batch               │
 service-  │   scales     │        └───┬─────────────┬──────────────┬─────────┘
 api)      │   alone)     │            │ingest       │page          │doc
            └──────┬───────┘            ▼             ▼              ▼
                   │            ┌────────────────────────────────────────────┐
                   │            │ vs-worker  (Firecracker microVMs, CPU only) │
                   │            │  role=ingest : detect, rasterize, text layer│
                   │            │  role=page   : classify → route → run DAG   │
                   │            │  role=doc    : stitch → extract → graph     │
                   │            └───────┬──────────────┬──────────────┬───────┘
                   │                    │ HTTP (OpenAI-compatible)    │
                   │                    ▼                             │
                   │            ┌──────────────────────┐              │
                   │            │ GPU model tier (vLLM)│              │
                   │            │ doc-parse VLMs,      │              │
                   │            │ general VLM, LLM,    │              │
                   │            │ embedders, classifier│              │
                   │            └──────────────────────┘              │
                   ▼                                                  ▼
       ┌──────────────────────┐   ┌───────────────┐   ┌──────────────────────┐
       │ Postgres             │   │ Object store  │   │ HelixDB              │
       │  ops schema (jobs)   │   │ (S3-compat.)  │   │ graph + vectors      │
       │  vault schema (DV2.0)│   │ originals,    │   │ ← RAG service reads  │
       │  registry (DAG lists)│   │ page images,  │   └──────────────────────┘
       └──────────────────────┘   │ page results  │
                                  └───────────────┘
```

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
                write vault (entities, assertions, evidence) + HelixDB (graph, vectors)
```

### 3.2 Adaptation flow (dev / stage only)

```
datasets (+ optional ground truth)
  └─▶ vs-adapt generate   : candidate DAGs from step catalogue × search space
  └─▶ vs-adapt run        : execute candidates on stage cluster
  └─▶ vs-adapt score      : per page class: quality metrics, latency, GPU-seconds
  └─▶ vs-adapt select     : best DAG per class under objective → DAG list + routing table
  └─▶ promote (manual)    : DAG list becomes active in prod registry
```

## 4. Component map

| Crate / folder | Kind | Responsibility | Spec |
|---|---|---|---|
| `vs-core` | lib | Shared types: IDs, hash keys, geometry, blocks, page profile, provenance, jobs | [SPEC](../crates/vs-core/SPEC.md) |
| `vs-infra` | lib | Task bus, blob store, config, telemetry | [SPEC](../crates/vs-infra/SPEC.md) |
| `vs-api` | bin | Public HTTP API: uploads, batches, status, DAG-list admin | [SPEC](../crates/vs-api/SPEC.md) |
| `vs-ingest` | lib | Format detection, rasterization, native text layer, CPU steps | [SPEC](../crates/vs-ingest/SPEC.md) |
| `vs-classify` | lib | Page classification → `PageProfile` | [SPEC](../crates/vs-classify/SPEC.md) |
| `vs-dag` | lib | DAG model, step trait, validation, executor | [SPEC](../crates/vs-dag/SPEC.md) |
| `vs-router` | lib | DAG list bundle (DAGs + routing table), DAG selection per page | [SPEC](../crates/vs-router/SPEC.md) |
| `vs-models` | lib | Model registry, OpenAI-compatible client, model-backed steps, output parsers | [SPEC](../crates/vs-models/SPEC.md) |
| `vs-stitch` | lib | Page results → one coherent document | [SPEC](../crates/vs-stitch/SPEC.md) |
| `vs-extract` | lib | Mentions, assertions, entity resolution | [SPEC](../crates/vs-extract/SPEC.md) |
| `vs-graph` | lib | Document graph model and builder | [SPEC](../crates/vs-graph/SPEC.md) |
| `vs-vault` | lib | Data Vault 2.0 writer + migrations | [SPEC](../crates/vs-vault/SPEC.md) |
| `vs-helix` | lib | HelixDB writer (nodes, edges, vectors) | [SPEC](../crates/vs-helix/SPEC.md) |
| `vs-worker` | bin | Worker process with roles `ingest`, `page`, `doc` | [SPEC](../crates/vs-worker/SPEC.md) |
| `vs-adapt` | bin | Workflow generation, scoring, selection | [SPEC](../crates/vs-adapt/SPEC.md) |
| `deploy/firecracker` | ops | microVM image and fleet | [SPEC](../deploy/firecracker/SPEC.md) |
| `deploy/gpu` | ops | Model serving tier | [SPEC](../deploy/gpu/SPEC.md) |
| `config/` | config | Example models, worker and DAG-list files | — |

Dependency direction (no cycles):
`vs-core` ← `vs-infra`, `vs-dag` ← `vs-router`, `vs-classify`, `vs-ingest`, `vs-models`
← `vs-stitch` ← `vs-extract` ← `vs-graph` ← `vs-vault`, `vs-helix` ← `vs-worker`, `vs-api`, `vs-adapt`.

## 5. Cross-cutting concerns

### 5.1 Identifiers and provenance

- **Hash keys** follow Data Vault conventions: `SHA-256` over normalised business keys
  joined with `|`, hex-encoded. Defined once in `vs-core::ids`.
  - `document_hk = H(tenant | sha256(original bytes))` → re-uploading the same file is idempotent.
  - `page_hk = H(document_hk | page_no)`
  - `block_hk = H(page_hk | run_id | block_index)` (blocks belong to a run; a new DAG gives new blocks)
  - `entity_hk = H(tenant | canonical_entity_key)`
  - `assertion_hk = H(run_id | predicate | sorted argument keys)`
- **Run** = one execution of the pipeline for one document under one DAG-list version.
  `run_id` is a UUIDv7.
- **Provenance** (attached to every block, mention and assertion):
  `run_id, dag_id@version, node_id, step_kind, model_id@revision, prompt_id@hash,
  params_hash, started_at, duration_ms, confidence`.
- **Evidence**: assertions point to `(block_hk, char_start, char_end)` spans and, for
  visual evidence, `(page_hk, bbox)`.

### 5.2 Scaling and throughput

- API is stateless; scale on request rate.
- Workers scale on JetStream consumer lag, separately per role and per lane.
- GPU pools scale per model on queue depth and batch fill.
- Page tasks are the unit of parallelism. Model calls are batched across pages and
  documents by vLLM continuous batching; workers keep many requests in flight.
- **Skipping work is the main lever**: born-digital pages with a good text layer skip
  OCR; pages without images skip figure steps; and so on. The routing table encodes this.

### 5.3 Priority lanes

Two lanes: `interactive` (small batches, reserved worker share, shorter timeouts) and
`batch` (everything else). Same code path; different streams and consumers.

### 5.4 Reliability

- At-least-once delivery; all writes idempotent via hash keys and `ON CONFLICT DO NOTHING`.
- Page task retries with backoff (default 3), then dead-letter. A document with failed
  pages proceeds to Stage 2 as `partial` when the failed fraction is ≤ a threshold (default 5 %).
- Every step has a timeout taken from the DAG node or step default.

### 5.5 Observability

`tracing` spans per job → page → node → model call; OpenTelemetry export; Prometheus
metrics for per-step latency, GPU-seconds per page, tokens, skip rates, routing decisions
by rule, and confidence distributions.

### 5.6 Security

Tenancy is part of every hash key. Workers have no inbound ports. Secrets come via the
environment / metadata service, never in DAG or config files. Originals are encrypted at
rest in the object store.

## 6. Decisions

| ID | Decision | Reason |
|----|----------|--------|
| D1 | CPU work in Firecracker, models on a separate GPU tier over HTTP | Firecracker has no GPU passthrough |
| D2 | One worker binary with roles | One rootfs image, simpler fleet |
| D3 | Model access through an OpenAI-compatible API | Works with vLLM (and SGLang, hosted APIs) without code changes |
| D4 | Workflows are static DAGs, chosen per page from a versioned DAG list | Reproducible; optimisation happens offline in `vs-adapt` |
| D5 | Data Vault is insert-only; reprocessing adds satellites with a new run | Natural DV2.0 history; old results stay traceable |
| D6 | HelixDB holds the *current* projection; the vault holds full history | RAG reads one consistent view |
| D7 | Structured model output via guided decoding (JSON schema) | Parseable output, fewer hallucinated formats |
| D8 | Large intermediate artefacts (page images, page results) live in the object store; queues carry references only | Small messages, no shared FS in Firecracker |
| D9 | HelixDB accessed only through HelixQL (`.hx` schema, migrations and queries in `vs-helix`); pinned to Helix 2.3.5 | Queries and data model versioned with the code; HelixQL is not available in Helix 3.x |

## 7. Assumptions (unanswered questions — please confirm or correct)

| ID | Assumption |
|----|-----------|
| A1 | "VLLM" means vision-language models served by **vLLM**. The client is OpenAI-compatible so other servers also work. |
| A2 | Self-hosted open-weight models by default. Hosted APIs can be registered as models but are off by default. |
| A3 | NVIDIA GPUs; exact type unknown. Model pools are configured per model. |
| A4 | Model servers run outside Firecracker (bare metal / Kubernetes). Cloud Hypervisor is an option if GPU isolation is required. |
| A5 | Relational DB is **PostgreSQL** (ops, registry and vault schemas). |
| A6 | Task bus is **NATS JetStream**; the `TaskBus` trait allows swapping in Kafka. |
| A7 | Object storage is S3-compatible. |
| A8 | HelixDB licensing (AGPL vs commercial/cloud) is settled on your side; we call compiled HelixQL queries over HTTP (D9). |
| A9 | Traceability at bbox + character-span level. |
| A10 | New entity/relation types are normalised automatically; new types get `review_status = pending` for optional later review. |
| A11 | No human review loop in v1; confidence and review status are stored. |
| A12 | Reprocessing adds new versions (D5); HelixDB is switched to the newest run per document. |
| A13 | DAG scoring = quality floor per page class, then weighted quality vs latency vs GPU cost. |
| A14 | Page classifier v1 = rules (tier 0) + one small vision classifier (tier 1). |
| A15 | Datasets may lack labels; then scoring uses agreement between strong models / a judge model, marked as lower-trust scores. |
| A16 | `vs-adapt` searches over a registered step catalogue and its parameters (model, prompt template, DPI, crop strategy), not free-form prompts. |
| A17 | Stage 2 is one pipeline in v1 with per-document-type prompt profiles; Stage 2 search is post-v1. |
| A18 | v1 formats: PDF, PNG, JPEG, TIFF (multi-page). Office formats via an optional converter in ingest. |
| A19 | The language hint (BCP-47) applies to all pages of a document in v1. |
| A20 | Per-call options: `language` (required), `priority`, optional `profile` (`quality`/`balanced`/`fast`), optional DAG-list version pin. |

## 8. Open questions

1. GPU hardware and the expected pages/day range (to size pools).
2. Which document types matter most after scientific papers (to seed adaptation datasets).
3. Ground-truth availability, especially relation-annotated scientific papers.
4. Retention rules for originals and page images.
5. Whether the RAG service needs anything beyond the HelixDB model in `vs-helix/SPEC.md`.

## 9. Research basis

- Document parsing VLMs: PaddleOCR-VL-1.5, MinerU2.5, dots.ocr, Logics-Parsing,
  olmOCR-2, DeepSeek-OCR, general VLMs (Qwen3-VL). Pipeline (layout → region recognition)
  and end-to-end page parsing both stay as step options; `vs-adapt` decides per class.
- Benchmarks for scoring: OmniDocBench, PureDocBench (2026), ParseBench (2026),
  Dr. DocBench (2026). Parsing is not solved (best ~74 on PureDocBench), so verification
  and confidence are required.
- Graph construction: KG4VD (2026) — entities/relations grounded to parser regions,
  cross-page entity alignment; MMGraphRAG — figures as scene graphs linked to text entities.
- Agentic navigation: SCoPE VLM (EACL 2026) — post-v1, query-time.
