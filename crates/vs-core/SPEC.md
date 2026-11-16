# vs-core — Specification

Shared domain types used by every other crate. No I/O, no async, no heavy dependencies.
Cross-cutting contracts: [`docs/SPEC.md`](../../docs/SPEC.md) §5.1.

## Status

Implemented: every type and hash-key constructor below, with golden-value tests for key
stability. 7 unit tests pass. This crate is stable — it is the bottom of the dependency
graph, so a breaking change here is a breaking change everywhere.

Remaining:
- [ ] `LanguageTag` validation is loose (non-empty, ASCII, `-` separated); it accepts
      strings that are not valid BCP-47. Tighten it or document the looseness as intended.

## Responsibilities

- Identifiers and Data Vault hash keys (`ids`).
- Geometry: normalised bounding boxes (`geometry`).
- Page-level output model: `Block`, `BlockKind`, `BlockContent`, `PageResult` (`block`).
- Page classification output: `PageProfile`, `DocType` (`page`).
- Provenance and evidence (`provenance`).
- Job request model: `JobSpec`, `Priority`, `Profile`, `JobState` (`job`).

## Rules

1. **Hash keys**: `HashKey::of(&[parts])` = lowercase hex SHA-256 of the parts, each
   trimmed, joined with `|`. Empty parts are encoded as an empty string, never skipped,
   so `["a", "", "b"]` ≠ `["a", "b"]`. Constructors for each key type live here so every
   crate computes them the same way. The formulas, with `H` = `HashKey::of`:

   | Key | Formula | Consequence |
   |-----|---------|-------------|
   | `document_hk` | `H(tenant \| sha256(original bytes))` | re-uploading the same file is idempotent |
   | `page_hk` | `H(document_hk \| page_no)` | stable across runs |
   | `block_hk` | `H(page_hk \| run_id \| block_index)` | blocks belong to a run; a new DAG gives new blocks |
   | `section_hk` | `H(heading_block_hk)` | run-scoped through its heading block |
   | `entity_hk` | `H(tenant \| canonical_entity_key)` | shared across runs and documents |
   | `assertion_hk` | `H(run_id \| predicate \| sorted argument keys)` | per-run, so reprocessing re-asserts |

   Note the asymmetry: `document_hk`, `page_hk` and `entity_hk` outlive a run, while
   `block_hk` and `assertion_hk` are scoped to one. Stores rely on this when dropping a
   partial run without touching shared rows.
2. **Bounding boxes** are normalised to `[0, 1]` relative to the page (origin top-left).
   Pixel coordinates are recovered with the page's render size. This keeps boxes stable
   when pages are re-rendered at another DPI.
3. **Plain-text projection**: every `Block` carries `text`, a plain-text rendering of its
   content (table → cell text in reading order, formula → LaTeX source, figure →
   description). Character spans in evidence refer to this field.
4. **Provenance is mandatory** on every `Block` and on every extraction output.
5. All public types are `Serialize + Deserialize`; JSON field names are `snake_case`.
   Enum tags use `#[serde(rename_all = "snake_case")]`. Changing a field name is a breaking
   change for stored page results and requires a `schema_version` bump in `PageResult`.

## Key types

| Type | Purpose |
|------|---------|
| `HashKey` | 64-char hex SHA-256 newtype |
| `RunId` | UUIDv7 newtype; one per document processing run |
| `LanguageTag` | BCP-47 string (validated loosely: non-empty, ASCII, `-` separated) |
| `BBox` | `{x0, y0, x1, y1}` normalised floats |
| `PageProfile` | Classifier output; input to the router |
| `Block` | One layout region with kind, bbox, reading order, content, text, confidence, provenance |
| `PageResult` | All blocks of one page from one run + the profile and DAG used |
| `Provenance` | Who/what produced an output (see §5.1 of the system spec) |
| `Evidence` | Span `(block_hk, char_start, char_end)` or region `(page_hk, bbox)` |
| `JobSpec` | What the API accepts per document |

## Versioning

`PageResult::SCHEMA_VERSION` starts at `1`. Readers must reject unknown major versions.

## Tests

Hash key stability (golden values), bbox validation, serde round-trips.
