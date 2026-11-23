# vs-vectors — Specification

The vector store: dense and sparse vectors for the blocks, sections and entities that
`vs-graph` emits as embed targets. A `VectorStore` trait with a **Qdrant** implementation,
so the store stays swappable. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) —
decision D9.

The graph itself lives in Postgres ([`vs-vault`](../vs-vault/SPEC.md), decision D6). This
crate holds only vectors and the payload needed to join a vector hit back to a graph node.

## Status

Implemented: the `VectorStore` trait (`ensure_ready`, `write_run`, `drop_run`,
`search_entities`, **`search`**, **`migrate_dimensions`**), `QdrantStore` over
`qdrant-client` 1.20 (gRPC), collection bootstrap behind an **alias** with the tenant
payload index and per-tenant HNSW layout, the pure-Rust sparse encoder, **hybrid search
fused inside Qdrant**, the `[qdrant]` config section and a devenv service. 9 unit tests
and 5 integration tests pass, the latter against a live Qdrant.

Verified against the server, not just asserted here: the collection carries a `dense`
named vector (size from the embedder, cosine) **and** a `sparse` one with
`modifier: idf`; `hnsw_config` is `m = 0` with `payload_m = 16`; `tenant` is a keyword
payload index with `is_tenant: true`; Qdrant accepts the UUID-shaped point ids derived
from graph keys; a tenant-filtered search returns the stored vector that
`search_entities` hands back for client-side scoring; and a dense query, a lexical query
and a fused one each return what only they can find.

**The lexical half is no longer written-but-unqueried.** `search` takes a dense vector, a
query text, or both, and when both are given each half is prefetched on its own index and
the server fuses the two candidate lists with reciprocal rank fusion. Fusion belongs
*inside* the store, in one request: a client handed two top-k lists cannot recover the
ranking it would have got from the union, and RRF needs no per-corpus tuning because a
cosine similarity and a BM25 score share neither scale nor distribution. The integration
test writes one block a dense embedder finds and one an identifier search finds, and
asserts each half finds its own and the fusion finds both.

**A dimension change is guarded and has a path.** `ensure_ready` reads the live
collection's vector size and refuses a mismatch with a message naming the way out — before
the guard, Qdrant refused each *point* whose vector was the wrong length, so a mismatch
surfaced as every document failing, one at a time, with nothing saying why. The
configured name is an **alias** over a collection carrying its dimension
(`vs_blocks__d1024`), so `migrate_dimensions` creates the next one and moves the alias,
and readers follow without being reconfigured. What it cannot do is convert the old
vectors: a different embedder is different geometry, so the documents have to be re-run,
and the report it returns says so. A collection that predates the alias is used directly
and left alone.

Remaining:
- [ ] **`pkgs.qdrant` does not build**, so the devenv process runs the official Docker
      image and needs `docker` on the host. nixpkgs builds Qdrant from source and its
      1.18.2 fails on an LLVM intrinsic signature mismatch in its AVX-512 quantization
      code; naming it in `packages` made the entire dev shell unbuildable, taking Postgres
      and Garage down with it.
- [ ] Retrieval quality is unmeasured. Both halves work and fuse; whether the fused
      ranking is better than either alone *on this corpus* is a question for a labelled
      retrieval set, and there is none. `vs-adapt` scores page parsing, not search.

### IDF across tenants: a decision, not a gap

`modifier: idf` makes Qdrant supply the inverse-document-frequency half of BM25 from the
collection's own statistics, which is what lets the lexical half work without an embedding
model. A collection holds every tenant's points, so those statistics are **shared**: a
term's rarity is computed across organizations.

No content crosses a tenant boundary — every query is filtered, and the filter is an
index with `is_tenant: true`, not a post-pass — but a ranking that depends on other
organizations' corpora is a statistical side channel, and saying "it is only ranking" is
not an argument that survives being asked about.

Per-tenant collections would fix it and cost more than it buys: they contradict the
payload-partitioned layout this collection is built for (`m = 0`, `payload_m = 16`), and a
deployment with thousands of organizations would have thousands of collections with a
Qdrant segment each. So the trade is offered instead: **`[qdrant].idf = false`** turns the
modifier off and the lexical half becomes saturated term frequencies alone — a weaker
ranking computed entirely from the document in front of it. It is a property of the
collection, so changing it means a new collection (the same alias move
`migrate_dimensions` performs).

## Collections

Three collections, mirroring the three vector kinds. They are separate rather than named
vectors in one collection because **their lifecycles differ**, and that difference is what
makes `drop_run` unambiguous:

| Collection | Point id | Lifecycle | Payload |
|---|---|---|---|
| `vs_blocks` | `block_hk` | per run; replaced on reprocessing, deleted by `drop_run` | `tenant`, `document_hk`, `run_id`, `page_no`, `model_ref` |
| `vs_sections` | `section_key` | per run; same | `tenant`, `document_hk`, `run_id`, `model_ref` |
| `vs_entities` | `entity_hk` | **corpus lifetime**; written once per entity, never dropped | `tenant`, `type_id`, `model_ref` |

That asymmetry is inherited deliberately: block and section vectors describe one run's
output, while an entity vector is the thing cross-document resolution searches against, so
dropping it would break the corpus. Re-running a document must not orphan entities that
other documents already link to.

Each collection carries a dense vector and a sparse vector (see below).

## Multitenancy

One collection per kind, **partitioned by payload**, which is Qdrant's own
recommendation for many tenants:

- `tenant` is a keyword payload index created with `is_tenant: true`, so Qdrant groups a
  tenant's points together on disk.
- The collection's HNSW config sets `m = 0` and `payload_m = 16`, so there is no global
  graph and one sub-graph per tenant. This is only correct because **every** query filters
  by tenant.
- Every search, upsert and delete must carry the tenant filter. A query without it
  searches every tenant's data. There is no server-side guarantee here — the filter is the
  isolation, so it belongs in the `VectorStore` implementation, never in the caller.

Unlike the HelixDB queries this replaces, Qdrant filters *before* the ANN search rather
than after, so there is no need to over-fetch `4·k` and trim. Ask for `k`.

## Sparse vectors

Lexical search moves here from HelixDB's BM25 index, so that exact-term matching — invoice
numbers, formula tags, citation keys, part numbers — still works alongside dense search.

**Sparse vectors are computed in-process, in Rust**: tokenise, fold to lowercase, emit
`{token_hash: term_frequency}`. The collection declares `modifier: idf`, so Qdrant applies
the IDF weighting server-side from its own corpus statistics. That is BM25-equivalent and
needs no model.

This is deliberately **not** BM42. BM42 derives term importance from a transformer's
attention and is distributed through FastEmbed, which is Python; adopting it would put a
Python dependency in a Rust-only pipeline (goal G9) or require serving another model. If
BM25-class lexical quality proves insufficient, the upgrade path is to serve a sparse
encoder in the GPU tier behind `vs-models` — the `VectorStore` interface does not change,
only where the sparse vector comes from.

IDF across tenants is a decision with a way out rather than a known limitation — see
[the Status section](#idf-across-tenants-a-decision-not-a-gap) and `[qdrant].idf`.

## Dimensions

The dense vector size is fixed per collection at creation time, taken from the embedding
model's `dimensions` in the model registry (`vs-models` already refuses to construct an
embedder for a model without it). Changing the embedding model changes the vector space,
so it means a **new collection, then switched** — never mixed vectors in one collection.
`model_ref` is in every payload so a mixed collection is at least detectable.

Both halves of that are implemented. The configured name (`vs_blocks`) is an **alias**
over a collection named for its dimension (`vs_blocks__d1024`), so:

- `ensure_ready` refuses a dimension that does not match the live collection, naming the
  command to run. Before the guard it left the collection alone and Qdrant refused each
  point instead, so a mismatch surfaced as every document failing one at a time.
- `migrate_dimensions(n)` creates the collection for `n`, moves the alias, leaves the old
  collection untouched for rollback, and reports that the documents have to be re-run —
  because nothing can convert a vector from one embedder's space to another's. That
  re-run is the "back-fill", and it is the caller's to schedule.
- A collection that predates the alias is used directly and never renamed: its points are
  perfectly good, and a migration nobody asked for is not an improvement. Migrating such
  a deployment means a new `collection_prefix` or dropping the collection first, and the
  error says so.

## Joining back to the graph

A search returns point ids, which are the graph's own keys (`block_hk`, `section_key`,
`entity_hk`). The caller resolves them in Postgres: a `block_hk` reaches
`sat_block_content` for text and bbox, `lnk_page_block` for its page, and
`sat_block_provenance` for the run, DAG, model and prompt that produced it. So every
vector hit still ends in a citation to a page region, which is the whole point (§5.1).

## Who reads this store

The external RAG service queries Qdrant and Postgres **directly** — there is no read API
in front of them. Consequences it owns, which HelixDB's compiled queries used to hide:

- applying the tenant filter on every call;
- choosing the lanes to fuse. `VectorStore::search` will do it — dense, lexical, or both
  fused server-side with RRF in one request — and a reader that would rather fuse its own
  way can still query the two named vectors directly;
- filtering to the current run, via `vault.v_current_run`, when a document has been
  reprocessed and two runs' blocks are both present.

The read contract is [`docs/STORES.md`](../../docs/STORES.md), which covers both stores
together — including the asymmetry that bites hardest here: **Postgres keeps every run of
a document and this store keeps only the latest.** The vault never deletes, while
`write_run` drops the document's previous points first. So a vector hit is always from the
current run, and a SQL join is not unless the caller filters. The same section records
that an entity's name vector is written once and never rewritten, because it is the target
every other document in the corpus was matched against.

## Tests

Integration tests are `#[ignore]`d and need Qdrant from `devenv up`, following
`vs-vault`'s and `vs-infra`'s pattern. The two that matter most, ported from the HelixDB
suite because they encode the invariants rather than the implementation:

1. **Run switch and retry.** Write run 1, assert its points are searchable; write run 2
   twice (proving idempotency); assert run 1's block and section points are gone, that
   entity points survive, and that a second tenant's search returns nothing.
2. **Resolver link vs create.** A known entity is linked, an unknown one is created, and
   two documents that both miss the search converge on the same deterministic key.
