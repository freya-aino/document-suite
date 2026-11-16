# vs-graph — Specification

Builds the **document graph**: one in-memory, store-independent graph per document run,
combining the structure layer (from stitching) and the semantic layer (from extraction).
Every store writes from this one structure, so no two stores can disagree about what was
extracted. This crate also owns the `GraphStore` trait, and depends on no store itself.
System spec: [`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: the node and edge model, `build()` over a `StitchedDocument` plus an
`ExtractionOutput`, embed-target emission, and `validate()`.

This crate is load-bearing — it is the single structure both stores are written from — and
it is the least tested crate in the workspace. Fix that before extending it.

Also implemented: **table cells and in-image text as nodes** (`page`), the `Cites` edge,
and the intra-page relations `vs-stitch` derives (`EnclosesRegion`, `ValueOf`). 21 tests,
12 of them against a whole built document — the builder had none before.

Remaining:
- [ ] Column flow is repaired by reordering `global_order` in `vs-stitch`, so the graph
      records the *corrected* reading order but not that a page had columns, nor which
      block sat in which column. A reader cannot tell a repaired two-column page from a
      single-column one.
- [ ] In-image text is matched to body text by case-insensitive containment on fragments
      of three characters or more. Deliberately conservative — a looser match would link a
      chart's "AUC" label to every "auction" — but it therefore misses inflections and
      any term the model transcribed differently from the body text.
- [ ] `validate()` leaves `Evidence`, `MatchesText` and `HasSection` endpoint-kind
      unconstrained, because each legitimately has two shapes. Those three could take a
      pair-of-alternatives rule instead of being exempt.
- [ ] Cell nodes come from re-parsing the table HTML, so a table the recognition model got
      structurally wrong yields confidently wrong cells. Nothing cross-checks the parse
      against the block's plain-text projection.

## Node kinds

| Kind | Key | Properties |
|------|-----|-----------|
| `document` | `document_hk` | title, language, page count, doc types, run id |
| `page` | `page_hk` | page no, profile, dag ref |
| `section` | heading `block_hk` | title, level |
| `block` | `block_hk` | kind, text, bbox, page no, global order, confidence, (table html / latex / figure description) |
| `entity` | `entity_hk` (canonical) or doc-local id before resolution | name, type id, raw type, aliases |
| `assertion` | `assertion_hk` | predicate id, raw predicate, kind, qualifiers, confidence, verified |
| `cell` | `cell:<block_hk>:<row>:<col>` | row, col, header, text, **column_header** — what makes a cell self-describing: "row 3, column 2" means nothing alone, the same cell carrying `column_header: "F1"` is citable evidence |
| `image_text` | `imgtext:<block_hk>:<n>` | text, index in the model's reading order |

Assertions are **nodes** (reified), so n-ary frames, qualifiers and multiple evidence
spans need no special cases. For fast traversal, binary assertions also get a direct
`related` edge between their subject and object entities.

## Edge kinds

| Edge | From → To | Layer |
|------|-----------|-------|
| `has_page` | document → page | structure |
| `has_section` | document → section (top level) / section → section | structure |
| `contains` | section → block | structure |
| `on_page` | block → page | structure |
| `next` | block → block (global reading order, body only) | structure |
| `continues_on` / `table_continues` | block → block | structure (stitch) |
| `caption_of` | caption block → figure/table block | structure (stitch) |
| `refers_to` | block → block | structure (stitch) |
| `mentions` | block → entity (props: char span, surface, confidence) | link |
| `arg` | assertion → entity (props: role) | semantic |
| `evidence` | assertion → block (props: char span) or assertion → page (props: bbox) | link |
| `encloses_region` | block → block, same page (props: coverage as confidence) | structure |
| `value_of` | value block → label block, form-like pages (props: confidence) | structure |
| `has_cell` | table block → cell | structure |
| `text_in_image` | figure/chart block → in-image text fragment | structure |
| `matches_text` | in-image text → block or entity (props: `via`) | link |
| `related` | entity → entity (props: predicate id, assertion_hk, confidence) | semantic shortcut |
| `cites` | block → entity (reference entry) | semantic |

Literal arguments (numbers, dates) are stored as properties on the `arg` list of the
assertion node (`literals: [{role, value, unit}]`), not as nodes.

## Embedding targets

The builder emits `EmbedTarget { node_key, kind, text }` for:
- body blocks with text (paragraphs, lists, captions; tables as linearised text),
- figure/chart blocks (description + in-image text; image embeddings post-v1),
- entities (name + type + one context sentence),
- sections (title + first paragraph) for coarse retrieval.

Stage 2 embeds them via `vs-models::Embedder`; the vector store persists them under these
same node keys, so a vector hit resolves straight back to a graph node.

## Invariants (checked by `DocGraph::validate`)

- every edge endpoint exists;
- every `assertion` has ≥ 1 `evidence` edge and ≥ 1 `arg` edge or literal;
- every `mentions` edge points from a block that exists in the same run;
- node keys are unique per kind.
