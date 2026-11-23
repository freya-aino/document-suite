# Store contract

What a reader — in practice the external RAG service — needs to know to query what this
pipeline writes. There is no read API in front of the stores (decision D5), so this
document is the interface.

Two stores, and the split is fixed:

| Store | Holds | Authority |
|-------|-------|-----------|
| **Postgres** (`vault`, `ops`, `registry` schemas) | the document graph, every extracted fact, and the provenance of each | system of record |
| **Qdrant** | the dense and sparse vectors of blocks, sections and entity names | derived; rebuildable from Postgres and the blob store |

Three things that used to be hidden inside compiled HelixQL queries are now **the
caller's responsibility**, and all three are easy to forget:

1. **Filter by tenant on every query.** Nothing in either store enforces it. In Postgres
   the tenant lives on `vault.hub_document.tenant_id` and `vault.hub_entity.tenant_id`
   and nowhere else, so a query starting from a block must join back to its document.
2. **Filter to the current run.** The vault is insert-only and keeps *every* run of a
   document. A query that forgets this returns the union of every reprocessing. Use
   `vault.v_current_run` (see below).
3. **Decide which lanes to search, and whether to fuse them.** `VectorStore::search` will
   do it for you — dense, lexical, or both fused inside Qdrant with reciprocal rank fusion
   in one request — and a reader querying Qdrant directly can do the same with a `prefetch`
   on each named vector and `fusion: rrf`. Two separate top-k lists cannot be fused back
   into the ranking the union would have given, so do it in the query.
4. **Check who may read what, if you are answering a person.** The vault says which
   *organization* a document belongs to and nothing about which of its members may see it:
   that is `ops.document_access` and `ops.document_grants` (decision D14), and
   `vs_vault::access::Reader::may_read` is the rule. A reader that joins the vault alone
   will happily return a colleague's private document.

## Keys

Every key is the lowercase hex SHA-256 of its parts, trimmed and joined with `|` (empty
parts are encoded, not skipped). `vs_core::HashKey` is the reference implementation, and
the formulas are normative — a reader that computes a key differently will not find the
row.

| Key | Formula | Lifetime |
|-----|---------|----------|
| `document_hk` | `H(tenant \| sha256(original bytes))` | forever; re-uploading a file is idempotent |
| `page_hk` | `H(document_hk \| page_no)` | forever |
| `block_hk` | `H(page_hk \| run_id \| block_index)` | one run |
| `section_hk` | `H(heading_block_hk)` | one run, through its heading block |
| `entity_hk` | `H(tenant \| canonical_entity_key)` | corpus lifetime, shared across documents |
| `entity_hk` (unresolved) | `H(document_hk \| local_id)` | one document |
| `assertion_hk` | `H(run_id \| predicate \| sorted argument keys)` | one run |
| `cell_hk` | `H("cell:<block_hk>:<row>:<col>")` | one run, through its block |
| `image_text_hk` | `H("imgtext:<block_hk>:<index>")` | one run, through its block |
| `run_hk` | `H(run_id)` | forever |

The last three are hashes of a *readable* key rather than of a list of parts. That is
narrowing, not derivation: the readable form is already unique, and hashing it is what
makes the column a `CHAR(64)` like every other key. Note the asymmetry in the lifetime
column — `document_hk`, `page_hk` and `entity_hk` outlive a run while the rest do not,
which is what lets a new run be written without disturbing anything shared.

## Postgres

### Finding the current run

```sql
SELECT run_id FROM vault.v_current_run WHERE document_hk = $1;
```

The view takes the newest `lnk_run_document` row per document. It does **not** check that
the run finished: join `vault.sat_run` for that.

```sql
-- The newest state of a run. `writing` means Stage 2's rows are in Postgres but the
-- vectors may not be in Qdrant yet.
SELECT DISTINCT ON (run_hk) state, started_at, finished_at
FROM vault.sat_run WHERE run_hk = $1 ORDER BY run_hk, load_dts DESC;
```

### Reading a satellite

Satellites are versioned, never updated: one row per change, ordered by `load_dts`. The
current value of anything is a `DISTINCT ON`:

```sql
SELECT DISTINCT ON (block_hk) block_hk, text, kind, bbox, content, confidence, furniture
FROM vault.sat_block_content
WHERE block_hk = ANY($1)
ORDER BY block_hk, load_dts DESC;
```

`vault.sat_mention` and `vault.sat_evidence` are *multi-active* satellites: several rows
share a `load_dts` and differ by `sub_seq`, so take the newest `load_dts` and then all of
its rows.

### The retrieval patterns

These are the eight queries the graph store used to answer as compiled procedures. They
are now the caller's to express; each is sketched here against the real schema.

**1. `SearchBlocks`** — vector search. Qdrant returns `block_hk`s (payload field `key`);
hydrate them with the `sat_block_content` query above. Tenant and run filtering happen in
Qdrant via the `tenant` and `run_id` payload fields.

**2. `SearchBlocksText`** — lexical search. The sparse vector half of the same Qdrant
collection; there is no full-text index in Postgres.

**3. `BlockContext`** — what surrounds a block.

```sql
-- Reading order is `global_order` over the current run's body blocks. `Next` edges are
-- not stored because this reconstructs them exactly.
SELECT DISTINCT ON (c.block_hk) c.block_hk, c.text, c.global_order
FROM vault.hub_block b
JOIN vault.sat_block_content c ON c.block_hk = b.block_hk
WHERE b.run_id = $1 AND NOT c.furniture
ORDER BY c.block_hk, c.load_dts DESC;
```

Continuations across pages are `vault.lnk_block_block` with `link_kind` in
`('continues_on', 'table_continues', 'caption_of', 'refers_to', 'encloses', 'value_of')`.
The last two are intra-page: `encloses` is geometric containment, `value_of` points from
a form value to its label.

**4. `EntityAssertions`** — what is claimed about an entity, with evidence.

```sql
SELECT a.assertion_hk, d.predicate_raw, d.predicate_id, d.literals, d.confidence,
       arg.role, ev.block_hk, e.char_start, e.char_end, e.bbox
FROM vault.lnk_assertion_arg arg
JOIN vault.hub_assertion a ON a.assertion_hk = arg.assertion_hk
JOIN LATERAL (
  SELECT * FROM vault.sat_assertion_detail s
  WHERE s.assertion_hk = a.assertion_hk ORDER BY s.load_dts DESC LIMIT 1
) d ON true
LEFT JOIN vault.lnk_assertion_evidence ev ON ev.assertion_hk = a.assertion_hk
LEFT JOIN LATERAL (
  SELECT * FROM vault.sat_evidence s
  WHERE s.lnk_hk = ev.lnk_hk ORDER BY s.load_dts DESC LIMIT 1
) e ON true
WHERE arg.entity_hk = $1 AND a.run_id = $2;
```

Evidence is a span when `block_hk` is set and a region when `page_hk` is — a `CHECK`
makes one of the two mandatory. `bbox` is
normalised to `[0, 1]` against the page, recoverable in pixels from
`sat_page_profile.render_width_px` / `render_height_px`.

**5. `RelatedEntities`** — the materialised shortcut for binary assertions.

```sql
SELECT l.to_entity_hk, r.predicate_id, r.confidence
FROM vault.lnk_entity_entity l
JOIN LATERAL (
  SELECT * FROM vault.sat_related s WHERE s.lnk_hk = l.lnk_hk ORDER BY s.load_dts DESC LIMIT 1
) r ON true
WHERE l.from_entity_hk = $1 AND l.run_id = $2;
```

**6. `DocumentOutline`** — the section tree and its blocks.

```sql
SELECT t.parent_section_hk, t.child_section_hk, s.title, s.level
FROM vault.lnk_section_section t
JOIN LATERAL (
  SELECT * FROM vault.sat_section x
  WHERE x.section_hk = t.child_section_hk ORDER BY x.load_dts DESC LIMIT 1
) s ON true
WHERE t.document_hk = $1 AND t.run_id = $2
ORDER BY s.level;
```

`parent_section_hk IS NULL` is a top-level section. Section membership is
`vault.lnk_section_block`. The document's own type is `vault.sat_document_type`
(`doc_type`, `confidence`, `source` ∈ `caller | model | page_vote`).

**7. `SearchEntityNames`** — the `{prefix}_entities` Qdrant collection. Dense search on
the entity name, filtered by `tenant`; this is what corpus resolution itself uses.

**8. `GetEntity`** — one entity with its newest labels.

```sql
SELECT h.entity_hk, h.canonical_key, l.name, l.type_id, l.raw_type, l.aliases,
       l.review_status, l.context
FROM vault.hub_entity h
JOIN LATERAL (
  SELECT * FROM vault.sat_entity_label s
  WHERE s.entity_hk = h.entity_hk ORDER BY s.load_dts DESC LIMIT 1
) l ON true
WHERE h.entity_hk = $1 AND h.tenant_id = $2;
```

`context` is the sentence the entity was last found in, capped at 400 characters and `''`
when there was none. `ORDER BY load_dts DESC LIMIT 1` gives the newest, which is the
newest *document's* wording: this satellite versions on a changed context, so the history
of an entity's label is also the list of ways it has been described. Reading all of it is
a plain query without the `LIMIT`.

### Components inside a block

A table is stored both as HTML (`sat_block_content.content`) and cell by cell, so one
number can be cited rather than "that table" — and an assertion's evidence *does* cite a
cell: `vault.sat_evidence` carries `row_idx` and `col_idx` where a table value is the
evidence, in place of `char_start`/`char_end`. One evidence row holds exactly one shape
(span, region or cell), enforced by a constraint.

```sql
SELECT c.row_idx, c.col_idx, s.text, s.header, s.column_header
FROM vault.hub_cell c
JOIN LATERAL (
  SELECT * FROM vault.sat_cell x WHERE x.cell_hk = c.cell_hk ORDER BY x.load_dts DESC LIMIT 1
) s ON true
WHERE c.block_hk = $1 ORDER BY c.row_idx, c.col_idx;
```

Text found inside a figure or chart is `vault.hub_image_text` + `vault.sat_image_text`,
and `vault.lnk_image_text_match` records where else in the document that text appears:
exactly one of `block_hk` and `entity_hk` is set, and `via` says which (`block_text` or
`entity_name`). Matching is case-insensitive containment on word boundaries, and both
sides must carry at least three characters — a deliberately conservative rule, because a
wrong edge in a provenance graph is worse than a missing one.

### Figures

`sat_block_content.content` for a figure or chart carries `crop_uri`, a blob-store key
(`crops/<page_hk>/<run_id>/<block_index>.png`) that resolves. The page it was cut from is
`pages/<document_hk>/<page_no>/<dpi>.png`, and the block's `bbox` is the region, so a
crop can also be recomputed rather than fetched.

## Qdrant

Three collections, named from the configured prefix (`VS__QDRANT__COLLECTION_PREFIX`,
default `vs`):

| Collection | Points | Payload |
|------------|--------|---------|
| `{prefix}_blocks` | one per embeddable body block | `tenant`, `key` (= `block_hk`), `model_ref`, `document_hk`, `run_id`, `page_no` |
| `{prefix}_sections` | one per section | `tenant`, `key` (= `sec:<heading_block_hk>`), `model_ref`, `document_hk`, `run_id` |
| `{prefix}_entities` | one per **newly created** entity | `tenant`, `key` (= `entity_hk`), `model_ref`, `type_id` |

Each point carries two named vectors:

- **`dense`** — the embedding model's output, cosine distance. Its dimension is fixed when
  the collection is created, from the embedder, and cannot change afterwards.
- **`sparse`** — term frequencies from a pure-Rust tokenizer, with the collection
  configured `modifier: idf` so Qdrant applies IDF server-side. BM25-class, not
  BM42-class. A text with no indexable token has no sparse vector at all rather than an
  empty one, so a sparse-only query will not return it.

Point ids are UUIDs derived from the node key, so writing the same run twice is an upsert
of the same points.

**Collection names are aliases.** The configured name (`vs_blocks`) points at a collection
carrying its vector dimension (`vs_blocks__d1024`), so a change of embedding model is an
alias move rather than a rebuild under the reader's feet — a reader always uses the
configured name. A deployment that predates the alias has a real collection under that
name instead, which works identically.

**Multitenancy is by payload filter.** `tenant` is a keyword index flagged `is_tenant`,
and the collections are built with `m = 0` / `payload_m = 16` so HNSW builds a subgraph
per tenant rather than one global graph. This is only correct because every query filters
by tenant — an unfiltered search over `m = 0` collections degrades badly and crosses
tenants while doing it.

## Asymmetries worth knowing before you query

- **Postgres keeps every run; Qdrant keeps only the latest.** There is no `DELETE`
  anywhere in the vault — superseded runs stay as history, which is the point of a Data
  Vault. `VectorStore::write_run` by contrast drops the document's previous points first.
  So vector search reaches only the current run, while SQL reaches all of them unless you
  filter. These two facts bite together: a vector hit is always current, a SQL join is
  not.
- **Entity vectors outlive runs.** A block or section vector is rewritten per run; an
  entity's name vector is written once, when the entity is created, and never rewritten —
  it is the target every other document in the corpus was matched against, so moving it
  would silently change past decisions.
- **IDF statistics are shared across tenants.** Payload-filter multitenancy means one
  collection, so the sparse half's IDF is computed over every tenant's text. No content
  crosses the boundary — the filter is an index, not a post-pass — but a ranking that
  depends on other organizations' corpora is a statistical side channel, which is why
  `[qdrant].idf = false` exists: it drops the modifier and the lexical half becomes
  saturated term frequencies computed from the document alone. A deployment that has set
  it reads *worse* lexical rankings on purpose, and a reader cannot tell from the data
  which choice was made — ask the operator.
- **A run can be `writing`.** Postgres rows and Qdrant points are not written in one
  transaction — nothing can span two stores. The vault records `writing` when its own
  rows are in and `done` or `partial` only once the vectors are too, so a run in state
  `writing` is one whose vectors may be missing. Treat it as incomplete rather than
  absent: the SQL is there and correct.
- **`crop_uri` is a key, not a URL.** So is `page_image_uri`. Resolve them against the
  configured bucket.
- **Derived objects expire; the vault's rows do not.** `[blob.retention]` prunes page
  images, results, crops and the scratch images handed to models on a schedule, while
  every row describing them stays. So a `crop_uri` or a `page_image_uri` from an old run
  may 404, and the fix is to re-run the document rather than to treat the row as wrong.
  Originals are kept forever by default, so a re-run is always possible.
