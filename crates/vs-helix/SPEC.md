# vs-helix — Specification

Writes the `vs-graph::DocGraph` and its embeddings to **HelixDB**, which the RAG service
queries. HelixDB holds the **current** projection (decision D6); history stays in the vault.

## Access: HelixQL only

HelixDB is addressed **only through HelixQL**: the schema and every query live in
`.hx` files in this repo, are compiled by the Helix CLI, and are called as named
endpoints (`POST {helix.url}/{QueryName}` with a JSON object of the query's
parameters). The JSON operation-tree requests / SDK DSL (`helix_db::dsl`) are not
used.

**Version pin: HelixDB 2.3.5 (CLI and server).** HelixQL is only supported on the 2.x
line; Helix 3.x archived it ("HelixQL v1 is archived and is not the current HelixDB
request model") and accepts only operation-tree requests. The CLI must be 2.3.5 or
newer within 2.x: earlier 2.x CLIs build against the upstream `main` branch (now
3.x) and fail; 2.3.5 checks out its own release tag. Upgrading past 2.x requires
revisiting this decision.

### Helix project (`crates/vs-helix/helix/`)

```
helix.toml                project + local instance (port 6969, MCP off, BM25 on)
db/0001_init_schema.hx    migration 0001: schema::1, the initial data model
db/000N_<change>.hx       later: schema::N + MIGRATION schema::N-1 => schema::N
db/queries.hx             every query the suite and the RAG service call
```

- **Migrations initialise the data model.** `schema::1` declares the model below; every
  later change adds `schema::N+1` and a `MIGRATION schema::N => schema::N+1 { … }`
  block mapping old fields to new (`N::Block => _:: { field: field, new_field: default }`).
  Deploying the project (`helix push <instance>`) applies pending migrations; no code
  path creates or alters schema at runtime.
- Queries are part of the deployed project, so a query change is a deploy, versioned
  with the code that calls it. `helix check` runs in CI.
- `HelixStore` (the `GraphStore` implementation) builds the parameters of each write
  query from the `DocGraph` (pure, unit-tested `Plan`) and calls the queries in
  batches of `batch_size`. The query names are the contract with the RAG service.
- Locally: `vs-helix-up` / `vs-helix-down` (devenv scripts) run `helix push dev` /
  `helix stop dev` (Docker). `helix check` also cargo-checks the generated Rust, which
  needs `pkg-config` and OpenSSL (in the devenv module).

### Helix 2.3.5 quirks (found while writing the queries)

- `DROP` takes an inline traversal, not a variable.
- `SearchBM25<T>(…)` cannot be followed by steps; assign it, then filter.
- The code generator singularises returned variable names into Rust closure
  parameters: a variable named `continues` becomes the keyword `continue` and the
  generated code does not compile. Avoid names whose singular is a Rust keyword.
- A lookup by key (`N<T>({field: v})`) is compiled as exactly-one: when nothing
  matches, the query fails with `No value found` (HTTP 500, `GRAPH_ERROR`) instead
  of returning empty. `WHERE`/`RANGE` do not change that; `ORDER` makes the value a
  list but not its type, and the generated code does not compile. So `UpsertN` after a
  lookup cannot create, and "not found" is an error the client interprets
  (`HelixError::is_not_found`).
- Secondary indexes are global per field name (shared by all node types), and lookups
  through a `UNIQUE INDEX` do not find stored nodes; plain `INDEX` lookups do. Key
  fields are therefore plain `INDEX`; uniqueness of the shared nodes comes from a
  separate write-only `UNIQUE INDEX uk` (`"document:<hk>"`, `"entity:<hk>"`), whose
  violation is `Duplicate key on unique index` (`HelixError::is_duplicate`).
- No conditionals and no string functions: "create if missing" is `Create*` (a
  duplicate means it exists) followed by `Update*`; `uk` is passed by the client.
- `helix push` while the port is still taken maps the next free host port but leaves
  the server on the configured one; `vs-helix-up` stops the instance first.
- `SearchV` results are returned as vectors with their properties and `data`; a `score`
  field, when present, is a distance whose scale depends on the index, so similarity is
  computed client-side from `data`.

## Data model in Helix

### Nodes (`N::`)

| Label | Key property | Other properties |
|-------|--------------|------------------|
| `Document` | `document_hk` | tenant, title, language, page_count, current_run_id, doc_types |
| `Page` | `page_key` (= page_hk + run id) | page_hk, document_hk, run_id, page_no, doc_type, dag_ref |
| `Section` | `section_key` | title, level |
| `Block` | `block_hk` | kind, text, page_no, bbox, global_order, confidence, content (table html / latex / figure description), run_id |
| `Entity` | `entity_hk` | tenant, name, type_id, aliases |
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

Vectors carry `model_ref` (the dimension is fixed per Helix instance by the served
embedding model; `Vectors::dimensions` is informational on the writer side); changing
the embedding model means a new index, back-filled, then switched — never mixed vectors
in one index.
BM25 full-text on `Block.text` and `Entity.name` for hybrid retrieval.

## Write algorithm (per document run, Stage 2)

`Document` and `Entity` are shared and upserted. Everything else of a document run —
pages, sections, blocks, assertions, their vectors and edges, and the `Related` edges
the run's assertions produced — is **run-scoped** (`run_id` on the node or edge; page
and assertion keys get the run id appended). HelixQL cannot upsert property-less
edges and has no conditionals, so idempotency comes from replacing a run as a whole:

1. `GetDocument` → previous `current_run_id` (`No value found` = new document).
2. `DropRun(run)` removes a partial earlier attempt of the same run (retry safety).
3. `CreateDocument` (duplicate = exists) + `UpdateDocument`. `CreateEntity` (entity +
   name vector + edge in one transaction) for entities corpus resolution created in
   this run (`RunInfo::new_entities`), then `UpdateEntities` in batches; a batch that
   hits an entity Helix does not have is redone per entity with create + update.
4. `AddPages`, `AddSections`, `LinkDocumentSections`, `LinkSubsections`, `AddBlocks`,
   `LinkContains`, `LinkNext` / `LinkContinuesOn` / `LinkTableContinues` /
   `LinkCaptionOf` / `LinkRefersTo`, `AddAssertions`, `LinkArgs`, `LinkEvidence`,
   `LinkMentions`, `LinkRelated`, `AddBlockVectors`, `AddSectionVectors`.
5. `SetCurrentRun(run)`.
6. `DropRun(previous)`.

Between 4 and 6 both runs exist; RAG queries that must not mix runs filter by
`Document.current_run_id`. Visual evidence (page + bbox, no text span) stays in the
vault only. `Entity.mention_count` from the earlier draft is not stored: a writer that
sees one document at a time cannot keep it correct; count `In<Mentions>` when needed.

## Corpus entity resolution (`HelixResolver`)

Implements `vs_extract::resolve::CorpusResolver` (`HelixResolver::new(store, embedder)`,
plus `with_judge(client, model)`, `with_thresholds(link, (low, high))`, `with_k`):

1. **Embed** `name (type)` + `\n` + context sentence with the same `Embedder` that fills
   the `entity_name` index (`HelixResolver::embed_text`; without context this is the
   `vs-graph` embed-target text).
2. **Search** `SearchEntityNames(tenant, vector, 4·k)` (k = 10). The query returns the
   matched `EntityName` vectors (with `entity_hk` and their `data`) and the entities
   behind them; the client joins them on `entity_hk` and computes **cosine** itself
   (Helix's own score is an index-dependent distance). A vector that comes back without
   `data` is re-embedded from `name (type)` as a fallback.
3. **Filter** to compatible types: same `type_id`, or either side has none
   (`compatible_type`). HelixQL cannot express "either unknown", so this is client-side.
4. **Score** = `0.6 · cosine + 0.4 · max Jaro-Winkler` over the normalised name and
   aliases of both sides (`normalise_name`: collapse whitespace, lowercase; own
   implementation, no crate). Keep the k best.
5. **Decide** per candidate in score order: `≥ link_threshold` (0.85) →
   `Resolution::Link { entity_hk, score }`; in `judge_band` (0.7..0.85) → LLM judge via
   `vs_models::OpenAiClient::chat` with JSON schema `{"same": bool}`, given both names,
   types, aliases and the new entity's context sentence; a `same: true` links. No judge
   configured, or every candidate rejected → `Resolution::New { entity_hk }` with
   `entity_hk = HashKey::entity(tenant, "<type_id>|<normalised name>")` =
   `H(tenant | type_id | normalised name)`, `type_id` falling back to the raw type.
   Candidates below the band stop the scan (the list is sorted).

Deterministic `New` keys mean two documents that both miss the search still meet in the
same Helix entity: `write_entities` treats the duplicate create as "exists". The caller
records each `New` with `RunInfo::note_resolution` so `write_run` creates exactly those
entities with their name vector (`CreateEntity`) and only updates the linked ones.

Not yet stored: the candidate's context sentence (Helix's `Entity` has none), so the
judge sees the stored entity's name, type and aliases only. Adding `context` to `Entity`
is a `schema::2` migration.

## Retrieval patterns for the RAG service (reference)

- **Passage search**: hybrid (vector `block_text` + BM25) → blocks → expand `Next`/
  `ContinuesOn` for context, `CaptionOf`/`RefersTo` for linked figures/tables.
- **Entity-centric**: vector `entity_name` → entity → `Arg`⁻¹ → assertions → `Evidence`
  → blocks (answers with citations to page + bbox).
- **Multi-hop**: entity → `Related` → entity …, with the assertion on each hop as evidence.
- **Scoped**: document → sections → blocks for "in paper X, section Y" questions.

Every answer can cite `block_hk` → page → bbox, which the vault resolves to the exact
run, DAG, model and prompt.
