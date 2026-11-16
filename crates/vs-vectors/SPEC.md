# vs-vectors — Specification

The vector store: dense and sparse vectors for the blocks, sections and entities that
`vs-graph` emits as embed targets. A `VectorStore` trait with a **Qdrant** implementation,
so the store stays swappable. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) —
decision D9.

The graph itself lives in Postgres ([`vs-vault`](../vs-vault/SPEC.md), decision D6). This
crate holds only vectors and the payload needed to join a vector hit back to a graph node.

## Status

Implemented: the `VectorStore` trait (`ensure_ready`, `write_run`, `drop_run`,
`search_entities`), `QdrantStore` over `qdrant-client` 1.20 (gRPC), collection bootstrap
with the tenant payload index and per-tenant HNSW layout, the pure-Rust sparse encoder,
the `[qdrant]` config section and a devenv service. 9 unit tests and 3 integration tests
pass, the latter against a live Qdrant.

Verified against the server, not just asserted here: the collection carries a `dense`
named vector (size from the embedder, cosine) **and** a `sparse` one with
`modifier: idf`; `hnsw_config` is `m = 0` with `payload_m = 16`; and `tenant` is a keyword
payload index with `is_tenant: true`. Qdrant accepts the UUID-shaped point ids derived
from graph keys, and a tenant-filtered search returns the stored vector that
`search_entities` hands back for client-side scoring.

This crate replaced the vector half of the HelixDB writer (decisions D6/D9).

Remaining:
- [ ] **`pkgs.qdrant` does not build**, so the devenv process runs the official Docker
      image and needs `docker` on the host. nixpkgs builds Qdrant from source and 1.18.2
      fails on an LLVM intrinsic signature mismatch in its AVX-512 quantization code;
      naming it in `packages` made the entire dev shell unbuildable, taking Postgres and
      NATS down with it.
- [ ] Sparse vectors are written but nothing queries them, so the lexical half is stored
      and unproven: no test fuses dense and sparse, because fusion belongs to the reader.
- [ ] No hybrid-search helper. Dense and sparse points are both written, but fusing them
      is left entirely to the reader (per decision 5), so nothing in-repo proves the two
      halves are jointly queryable.
- [ ] IDF statistics are shared across tenants (see "Sparse vectors"); no per-tenant
      scoring.
- [ ] A dimension change still means manually creating and back-filling a new collection;
      there is no migration helper.
- [ ] `ensure_ready` creates collections on first use, so the first `doc` worker to start
      decides the dimension for everyone. A deliberate dimension change needs the old
      collections dropped first, which nothing guards.

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

Known limitation: with payload-partitioned multitenancy, IDF statistics are computed over
**all** tenants' points in a collection, not per tenant. A term that is rare for one tenant
but common overall is scored as common. Acceptable while tenants hold similar document
mixes; revisit with per-tenant scoring if that stops being true.

## Dimensions

The dense vector size is fixed per collection at creation time, taken from the embedding
model's `dimensions` in the model registry (`vs-models` already refuses to construct an
embedder for a model without it). Changing the embedding model changes the vector space,
so it means a **new collection, back-filled, then switched** — never mixed vectors in one
collection. `model_ref` is in every payload so a mixed collection is at least detectable.

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
- fusing dense and sparse results (reciprocal rank fusion is the expected default);
- filtering to the current run, via `vault.v_current_run`, when a document has been
  reprocessed and two runs' blocks are both present.

The read contract is documented in [`vs-vault/SPEC.md`](../vs-vault/SPEC.md) together with
the SQL side.

## Tests

Integration tests are `#[ignore]`d and need Qdrant from `devenv up`, following
`vs-vault`'s and `vs-infra`'s pattern. The two that matter most, ported from the HelixDB
suite because they encode the invariants rather than the implementation:

1. **Run switch and retry.** Write run 1, assert its points are searchable; write run 2
   twice (proving idempotency); assert run 1's block and section points are gone, that
   entity points survive, and that a second tenant's search returns nothing.
2. **Resolver link vs create.** A known entity is linked, an unknown one is created, and
   two documents that both miss the search converge on the same deterministic key.
