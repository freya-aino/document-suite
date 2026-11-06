# vs-helix — Specification

Writes the `vs-graph::DocGraph` and its embeddings to **HelixDB**, which the RAG service
queries. HelixDB holds the **current** projection (decision D6); history stays in the vault.

> Check against the Helix version you run. Recent Helix releases take queries as a JSON
> AST built by the Rust SDK (`helix-db` crate, `helix_db::dsl::prelude`) and sent to
> `POST /v1/query`; older ones used compiled HelixQL. This crate hides that behind the
> `GraphStore` trait, so only `HelixStore` changes.

## Data model in Helix

### Nodes (`N::`)

| Label | Key property | Other properties |
|-------|--------------|------------------|
| `Document` | `document_hk` | tenant, title, language, page_count, current_run_id, doc_types |
| `Page` | `page_hk` | page_no, doc_type, dag_ref |
| `Section` | `section_key` | title, level |
| `Block` | `block_hk` | kind, text, page_no, bbox, global_order, confidence, content (table html / latex / figure description), run_id |
| `Entity` | `entity_hk` | tenant, name, type_id, aliases, mention_count |
| `Assertion` | `assertion_key` | predicate_id, predicate, kind, qualifiers, literals, confidence, verified, run_id |

`tenant` is on every node; every query filters by it.

### Edges (`E::`)

Same kinds as `vs-graph` (`HasPage`, `HasSection`, `Contains`, `OnPage`, `Next`,
`ContinuesOn`, `TableContinues`, `CaptionOf`, `RefersTo`, `Mentions`, `Arg`, `Evidence`,
`Related`, `Cites`), with the edge props from the graph (char spans, roles, confidence,
predicate ids).

### Vectors (`V::`)

| Index | Attached to | Source text |
|-------|-------------|-------------|
| `block_text` | Block | block text (tables linearised; figures: description + in-image text) |
| `section_text` | Section | title + first paragraph |
| `entity_name` | Entity | `name (type)` + context sentence; also used for corpus resolution |

Vectors carry `model_ref` and `dimensions`; changing the embedding model means a new
index, back-filled, then switched — never mixed vectors in one index.
BM25 full-text on `Block.text` and `Entity.name` for hybrid retrieval.

## Write algorithm (per document run, Stage 2)

1. Upsert `Entity` nodes by `entity_hk` (canonical; shared across documents) — update
   name/aliases/mention_count.
2. Insert the new run's `Document` (upsert) / `Page` / `Section` / `Block` / `Assertion`
   nodes and all edges, with vectors.
3. Set `Document.current_run_id` to the new run.
4. Delete the previous run's `Section` / `Block` / `Assertion` nodes of that document
   and their edges (entities stay).

Steps 2–4 in one transaction where Helix supports it. If not: RAG queries filter blocks
and assertions by `run_id = Document.current_run_id`, and step 4 runs asynchronously.

Writes are batched (`batch_size` nodes/edges per request, default 1 000) and idempotent:
keys are deterministic, so a retried write upserts the same objects.

## Corpus entity resolution (`HelixResolver`)

Implements `vs_extract::resolve::CorpusResolver`:

1. vector search `entity_name` top-k (k = 10) within tenant, filtered by compatible type;
2. score = 0.6 × cosine + 0.4 × name similarity (normalised Jaro-Winkler on name/aliases);
3. ≥ `link_threshold` → link; in `judge_band` → LLM judge (via `vs-models`) with both
   entities' names, types and context sentences; else new entity
   (`entity_hk = H(tenant | type_id | normalised name)`).

## Retrieval patterns for the RAG service (reference)

- **Passage search**: hybrid (vector `block_text` + BM25) → blocks → expand `Next`/
  `ContinuesOn` for context, `CaptionOf`/`RefersTo` for linked figures/tables.
- **Entity-centric**: vector `entity_name` → entity → `Arg`⁻¹ → assertions → `Evidence`
  → blocks (answers with citations to page + bbox).
- **Multi-hop**: entity → `Related` → entity …, with the assertion on each hop as evidence.
- **Scoped**: document → sections → blocks for "in paper X, section Y" questions.

Every answer can cite `block_hk` → page → bbox, which the vault resolves to the exact
run, DAG, model and prompt.
