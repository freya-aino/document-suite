# vs-vault — Specification

Writes everything the pipeline produces into PostgreSQL (assumption A5), in three schemas:

| Schema | Purpose | Mutability |
|--------|---------|-----------|
| `ops` | job and page-task state for the API and workers | mutable (operational) |
| `registry` | DAG lists, ontology (entity / relation types) | mutable, versioned rows |
| `vault` | **Data Vault 2.0 raw vault**: the system of record and audit trail | insert-only |

Migrations: `migrations/0001_ops_registry.sql`, `migrations/0002_vault.sql`
(run with `sqlx migrate run`; embedded via `sqlx::migrate!` in the worker and API).

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
| `lnk_assertion_evidence` | assertion, block | — |

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

## Operational state (`ops` module)

`Ops` wraps the `ops` schema for the API and the workers: `create_job`, `get_job`
(tenant-scoped), `set_state`, `start_pages` (end of ingest: page count, page tasks),
`page_started`, `page_finished`. `page_finished` moves the page task to its final
state and bumps the job counters in **one statement**; only the first call per page
counts, so redeliveries cannot double count and exactly one caller sees
`completed_job()` and publishes the doc task.

## Tests

`tests/vault.rs` uses `#[sqlx::test]` (fresh migrated database per test) against the
devenv Postgres; the `vs` role gets `CREATEDB` from a devenv task. Run with
`cargo test -p vs-vault -- --ignored`.

## Performance notes

- `vault` tables partitioned by `load_dts` month for satellites with high volume
  (`sat_block_content`, `sat_mention`).
- Workers write through a bounded pool (`postgres.max_connections` per worker); with many
  microVMs, put PgBouncer (transaction mode) in front.
