# vs-vault — Specification

Writes everything the pipeline produces into PostgreSQL (assumption A5), in three schemas:

| Schema | Purpose | Mutability |
|--------|---------|-----------|
| `ops` | job and page-task state for the API and workers | mutable (operational) |
| `registry` | DAG lists, ontology (entity / relation types) | mutable, versioned rows |
| `vault` | **Data Vault 2.0 raw vault**: the system of record and audit trail | insert-only |

Migrations: `migrations/0001_ops_registry.sql`, `migrations/0002_vault.sql`,
`migrations/0003_app_role_privileges.sql`, `migrations/0004_api_ops.sql` (idempotency
keys, job state log, page-level evidence), `migrations/0005_graph.sql` (sections, the
`related` shortcut, citations) and `migrations/0006_document_type.sql`, embedded via
`sqlx::migrate!`, plus `migrations/0007_page_components.sql` (table cells, in-image text
and its matches).

System spec: [`docs/SPEC.md`](../../docs/SPEC.md) — decisions D5 (insert-only history)
and D6 (this is the single system of record *and* the graph projection).

## Status

Implemented: the full raw vault (now 9 hubs, 12 links, 12 satellites), the `ops` schema
(jobs, batches, page tasks, state log, idempotency keys), the `registry` schema (DAG
lists, entity and relation types), role separation between the migration owner and the
runtime role, `write_document`, `write_page`, `write_stage2`, `finish_run`, the
`v_current_run` view, the graph layer (`graph.rs`, written inside `write_stage2`'s
transaction) and corpus entity resolution (`PgCorpusResolver`).
15 unit tests, plus 17 Postgres integration tests (`#[ignore]`d; they need a database).
All seven migrations apply, and every write path has been executed: `tests/graph.rs`
covers the graph layer across all of 0005 to 0007 and `PgCorpusResolver` against real
label rows.

Running them for the first time found three defects that no unit test could have caught:

1. `write_stage2` never pushed `assertion_hk` into its column array. `UNNEST` over arrays
   of unequal length pads the short ones with NULL, so **every** assertion insert violated
   `hub_assertion`'s NOT NULL — the assertion write path had never worked. The dedup guard
   above it was dead for the same reason.
2. `insert_sat_run` had `finished_at` (`Utc::now()`) inside its `hash_diff`, so a
   redelivered doc task versioned the satellite again: the wall clock moving was recorded
   as the run having changed.
3. `sat_image_text`'s insert aliased its key column `hk`, while `latest_differs` generates
   `s.<parent> = v.<parent>` — so the predicate referenced a column that did not exist.

The first two predate the storage rewrite and had been latent since Stage 2 was written.

This is now the suite's only graph store (D6), and it was already richer than the one it
replaced: it keeps **both** span and region evidence, full `load_dts` history, block
provenance, and the DAG and model hubs — none of which the graph projection held.

### One transaction, one entry point

`write_stage2` takes the `DocGraph` and writes the graph layer inside its own
transaction. It used to be two calls, `write_stage2` then `PgGraphStore::write_run`, with
a commit between them, and that cost two things:

- a run could be **recorded as finished before its graph existed** — a crash between the
  two commits left `sat_run` saying `done` over a document with no sections, cells or
  in-image text, which no reader could tell apart from a complete run;
- two writers for one graph meant two projections that could drift. Defect 1 above was
  fixed in one of them and had no counterpart in the other.

The state machine that replaces it: `write_stage2` writes every Postgres row and marks
the run `writing`; the vectors go to Qdrant; `finish_run` then records `done` or
`partial`. Nothing can span two stores, so `writing` is the honest intermediate state and
readers are told to treat it as "SQL complete, vectors possibly not" —
[`docs/STORES.md`](../../docs/STORES.md) says so.

Remaining:
- [ ] **The judge cannot see a candidate's context sentence.** `sat_entity_label` has no
      `context` column, so `PgCorpusResolver`'s judge sees only the stored name, type and
      aliases — the same blind spot the previous store had, and the one Postgres could
      actually fix. It needs the column plus the context threaded into `write_stage2`.
- [ ] Applying the new migrations is a deploy-ordering constraint: `check_migrations`
      refuses to start a worker whose database is missing any embedded migration, so
      `vs-migrate` has to run before any binary built with 0005 to 0007.
- [ ] Nothing checks that a satellite's `hash_diff` excludes write-time values. Defect 2
      above was exactly that mistake, and the same trap exists in every satellite whose
      descriptive set might later gain a timestamp.
- [x] The read contract for the external RAG service is
      [`docs/STORES.md`](../../docs/STORES.md): the key formulas, all eight retrieval
      patterns as SQL against this schema, the Qdrant layout, and the three things that
      became the caller's responsibility when the compiled queries went away — tenant
      scoping, current-run filtering and dense/sparse fusion. Every SQL block in it has
      been `PREPARE`d against a migrated database, so the column names are real.
- [ ] Table cells are stored per `(block, row, col)`, so a table whose structure changes
      between runs leaves the previous run's cells addressable at the same coordinates
      with different content. The satellite versions them, but a reader taking the latest
      row gets the newest run's value, which is usually what is wanted and occasionally
      not.
- [ ] High-volume satellites are not partitioned (see "Performance notes").
- [ ] Business vault and information marts (PIT and bridge tables) are out of scope for
      v1; the raw vault is shaped so they can be derived later.

Deliberate non-goals here:
- `Next` (reading order) has no link table and needs none: it is reconstructible from
  `sat_block_content.global_order` plus `furniture`, so a table would be a second,
  divergeable copy.
- No vectors (D9) — they live in [`vs-vectors`](../vs-vectors/SPEC.md).

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A5 | Relational DB is **PostgreSQL** (`ops`, `registry` and `vault` schemas). Schema names are fixed, not configurable: the suite expects to own them in whichever Postgres instance it is pointed at. |
| A10 | New entity/relation types are normalised automatically; new types get `review_status = pending`. |
| A12 | Reprocessing adds new versions (D5); readers that want one consistent view use `v_current_run`. |

## Roles and migrations

| Role | Used by | Privileges |
|------|---------|-----------|
| schema owner (`vs` locally) | the migration deploy step, tests | owns `ops`, `registry`, `vault` |
| `vs_app` | API and workers at runtime | `vault`: SELECT, INSERT only (no UPDATE/DELETE/TRUNCATE); `ops`, `registry`: SELECT, INSERT, UPDATE; no DDL |

- Migrations are a **deploy step**: `vs-api migrate` (owner credentials in
  `DATABASE_URL`) or `sqlx migrate run`. The API and workers never migrate; at startup
  they call `check_migrations`, which refuses to start unless every embedded migration
  is applied with a matching checksum.
- `vs_app` must exist before migrating (the platform creates it; devenv's
  `vs-vault:roles` task does locally). Migration 0003 fails loudly if it is missing, and
  sets default privileges so later tables get the same grants.
- The insert-only rule of the raw vault (D5) is therefore enforced by the database,
  not only by the writer; a test asserts that UPDATE/DELETE/TRUNCATE/DDL are denied.

## Raw vault model

Conventions: hash keys `CHAR(64)` from `vs-core::ids`; every row has `load_dts
TIMESTAMPTZ` and `record_source TEXT`; satellites have `hash_diff CHAR(64)` (SHA-256 of
the descriptive columns) and are inserted only when `hash_diff` changed. No updates,
no deletes (retention is handled by partition drops, if required).

`record_source` format: `vs/<component>/<dag_ref or stage2@ver>/<model_ref or cpu>`.

### Hubs

| Hub | Business key |
|-----|--------------|
| `hub_document` | tenant, content sha256 |
| `hub_page` | document hk, page no |
| `hub_block` | page hk, run id, block index |
| `hub_entity` | tenant, canonical entity key |
| `hub_assertion` | run id, predicate, sorted argument keys |
| `hub_run` | run id |
| `hub_dag` | dag id, version |
| `hub_model` | model id, revision |

### Links

| Link | Connects | Dependent child key |
|------|----------|---------------------|
| `lnk_document_page` | document, page | — |
| `lnk_page_block` | page, block | — |
| `lnk_run_document` | run, document | — |
| `lnk_block_dag_model` | block, dag, model (nullable) | node id |
| `lnk_block_block` | block, block | link kind (stitch) |
| `lnk_mention` | block, entity | — |
| `lnk_assertion_arg` | assertion, entity | role |
| `lnk_assertion_evidence` | assertion, block **or** page (`Evidence::Region`) | — |

### Satellites

| Satellite | Parent | Content |
|-----------|--------|---------|
| `sat_document_meta` | hub_document | external ref, filename, size, format, language hint, page count |
| `sat_run` | hub_run | DAG-list version, profile, priority, final state, timings |
| `sat_page_profile` | hub_page | run id, profile JSON, dag ref, routing rule, render size |
| `sat_block_content` | hub_block | kind, bbox, reading/global order, text, content JSON, level, confidence, furniture flag |
| `sat_block_provenance` | hub_block | provenance JSON |
| `sat_entity_label` | hub_entity | name, type id, raw type, aliases, review status |
| `sat_assertion_detail` | hub_assertion | raw predicate, predicate id, kind, qualifiers, literals, confidence, verified, provenance |
| `sat_mention` (multi-active by `sub_seq`) | lnk_mention | run id, char span, surface, confidence |
| `sat_evidence` (multi-active by `sub_seq`) | lnk_assertion_evidence | char span or bbox |

Multi-active satellites share one `hash_diff` across the rows of a parent (hash of the
whole set), so a changed set is re-inserted as a whole under a new `load_dts`.
| `sat_stitch_link` | lnk_block_block | run id, confidence |

### Reprocessing (D5 / A12)

A new run creates new blocks and assertions (their keys include the run id), new
satellites on page/document/entity hubs, and a new `hub_run`. Older runs stay queryable.
"Current" = latest run per document, exposed by the view `vault.v_current_run`.
Business vault / information marts (PIT, bridge tables) are out of scope for v1 but
the raw vault is designed so they can be derived.

## Write points

| When | Writer call | Tables |
|------|-------------|--------|
| ingest done | `write_document` | hub_document, sat_document_meta, hub_run, sat_run, lnk_run_document, hub_page, lnk_document_page |
| each page done (Stage 1) | `write_page` | sat_page_profile, hub_block, lnk_page_block, sat_block_content, sat_block_provenance, hub_dag, hub_model, lnk_block_dag_model |
| Stage 2 done | `write_stage2` | lnk_block_block + sat_stitch_link, hub_entity + sat_entity_label, lnk_mention + sat_mention, hub_assertion + sat_assertion_detail, lnk_assertion_arg, lnk_assertion_evidence + sat_evidence, sat_run (final) |

All writes for one call run in **one transaction**, batched with `UNNEST` arrays
(one statement per table), `ON CONFLICT DO NOTHING` on hubs/links. Satellites use
`INSERT … SELECT … WHERE NOT EXISTS (same parent with same latest hash_diff)`.

Hash keys for runs, DAGs, models and links come from `vs-core` (`HashKey::run`,
`HashKey::dag`, `HashKey::model`, `HashKey::link`). `write_document` takes a `RunMeta`
whose `started_at` is the job creation time, so retries do not add `sat_run` rows.

`write_stage2(run, spec, doc, ext, final_state)` (all implemented):

- Entity key: `DocEntity.entity_hk` when the corpus resolver set it (then
  `canonical_key` = lowercased name, the resolver's business key is not carried on the
  struct), else the document-local key `H(tenant | document_hk | local_id)` with
  `canonical_key = local:<document_hk>:<local_id>` (`local_entity_key`). Entities with a
  normalised `type_id` get `review_status = approved`, others `pending`.
- Assertion key: `HashKey::assertion(run, predicate, arg keys)` where literal arguments
  take part as `H(role | value | unit)`, so two attribute assertions with the same
  predicate but different values stay distinct. Literals are stored in `literals`.
- Mentions are linked per `(block, entity)`; evidence per `(assertion, block)` or
  `(assertion, page)` for regions (`block_hk` is nullable since 0004).
- `record_source` is `vs/doc/stage2@1/<model or cpu>`; the final `sat_run` row carries
  `final_state` (`done` / `partial`) and `finished_at`.

## Operational state (`ops` module)

`Ops` wraps the `ops` and `registry` schemas for the API and the workers, plus the
vault's read side for the API:

- Jobs: `create_job`, `create_job_for_document` (hash key known up front),
  `create_jobs` (bulk, batches), `get_job` (tenant-scoped), `set_state`, `start_pages`
  (end of ingest: page count, page tasks), `page_started`, `page_finished`,
  `job_timings` (created/updated + `ops.job_state_log`, written by `create_jobs`,
  `set_state` and `start_pages` on every state change), `current_run` (newest job of a
  tenant for a document; `skip_if_processed`). `page_finished` moves the page task to
  its final state and bumps the job counters in **one statement**; only the first call
  per page counts, so redeliveries cannot double count; exactly one caller sees
  `completed_job()`, while `all_finished()` is true for every call once the last page is
  final (the page worker publishes the doc task on `all_finished()`, so a redelivery
  after a lost publish still enqueues it).
- Batches: `create_batch`, `get_batch` (counts per state + a page of jobs).
- Documents (vault reads): `document_run` (tenant check via `hub_document`, resolves
  `run_id` or `v_current_run`), `document_summary`.
- Idempotency: `get_idempotent` / `put_idempotent` (`ops.idempotency_keys`, 24 h; a live
  entry is never overwritten), `purge_idempotency_keys`.
- DAG lists: `dag_lists_list`, `dag_list_insert_candidate`, `dag_list_activate` (one
  active per scope; retires the previous one in the same transaction under an advisory
  lock), `dag_list_get`, `active_dag_list` / `active_dag_list_version` (tenant's, else
  global).
- Ontology: `entity_types`, `relation_types`, `insert_pending_entity_type`,
  `insert_pending_relation_type` — plain rows. `PgOntologyStore` (`ontology.rs`)
  implements `vs_extract::OntologyStore` over them for the `doc` worker; rows with
  `merged_into` set are hidden so the normaliser only ever returns a live type id.

## Tests

`tests/vault.rs` uses `#[sqlx::test]` (fresh migrated database per test) against the
devenv Postgres; the `vs` role gets `CREATEDB` from a devenv task. Privilege tests
connect to the test database as `vs_app`. Run with
`cargo test -p vs-vault -- --ignored`.

## Performance notes

- High-volume satellites (`sat_block_content`, `sat_mention`) **should** be partitioned by
  `load_dts` month. This is not implemented — no migration declares a partition — so it is
  a sizing decision still to be made, not a property of the current schema.
- Workers write through a bounded pool (`postgres.max_connections` per worker); with many
  microVMs, put PgBouncer (transaction mode) in front.
