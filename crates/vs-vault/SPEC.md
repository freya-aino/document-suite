# vs-vault — Specification

Writes everything the pipeline produces into PostgreSQL (assumption A5), in three schemas:

| Schema | Purpose | Mutability |
|--------|---------|-----------|
| `ops` | job and page-task state for the API and workers | mutable (operational) |
| `registry` | DAG lists, ontology (entity / relation types) | mutable, versioned rows |
| `vault` | **Data Vault 2.0 raw vault**: the system of record and audit trail | insert-only |

Migrations: `migrations/0001_ops_registry.sql`, `migrations/0002_vault.sql`,
`migrations/0003_app_role_privileges.sql`, `migrations/0004_api_ops.sql` (idempotency
keys, job state log, page-level evidence), embedded via `sqlx::migrate!`.

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

- `vault` tables partitioned by `load_dts` month for satellites with high volume
  (`sat_block_content`, `sat_mention`).
- Workers write through a bounded pool (`postgres.max_connections` per worker); with many
  microVMs, put PgBouncer (transaction mode) in front.
