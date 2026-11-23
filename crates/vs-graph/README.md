# vs-graph

Builds the **document graph**: one in-memory, store-independent graph per document run,
combining the structure layer (from stitching) and the semantic layer (from extraction).
Every store writes from this one structure, so no two stores can disagree about what was
extracted. It depends on no store itself: `RunInfo` and `Vectors` — what a store needs
about a run beyond the graph — live here, and the store traits live with their backends
(`vs_vectors::VectorStore`; the vault writes the graph layer in its own Stage 2
transaction).
System spec: [`docs/SPEC.md`](../../docs/SPEC.md).

## What it does

Builds the graph: the node and edge model, `build()` over a `StitchedDocument` plus an
`ExtractionOutput`, embed-target emission, and `validate()`. Every store is written from
this one structure, so no two stores can disagree about what was extracted.

This crate is load-bearing — it is the single structure both stores are written from — so
it is tested before it is extended.

Also implemented: **table cells and in-image text as nodes** (`page`), the `Cites` edge,
and the intra-page relations `vs-stitch` derives (`EnclosesRegion`, `ValueOf`,
`FootnoteOf`). 23 tests, 12 of them against a whole built document — the builder had none
before.

**A table value's evidence points at its cell.** `Evidence::Cell { block_hk, row, col }`
becomes an `Evidence` edge to the `Cell` node for that row and column, so "which number is
this claim about" resolves to one cell rather than to a table. A character span into the
table's flattened text could not do that: `92.1` twice in a results table gives two
identical spans.

## Known limitations

- Column flow was repaired by reordering `global_order`, which recorded the *corrected*
  reading order and erased the shape of the page it corrected. A block now carries
  `column` and `page_columns`, and the vault stores both (`sat_block_placement`), so a
  repaired two-column page is distinguishable from a single-column one and the blocks that
  shared a column can be found. A *spanning* block belongs to no column and says so with
  `None`, rather than being put in the column it happens to start in.
- In-image text is matched to body text by case-insensitive containment **on word
  boundaries**, over fragments of three characters or more, and the same floor applies to
  the entity side: an entity named "IT" or "AI" is never matched. Both rules exist because
  plain containment produced edges that were simply wrong — "IT" was found inside
  "monitoring", and an entity with an empty name matched every fragment. Deliberately
  conservative — a looser match would link a chart's "AUC" label to every "auction" — but
  it therefore misses inflections and
  any term the model transcribed differently from the body text.
- `validate()` leaves `Evidence`, `MatchesText` and `HasSection` endpoint-kind
  unconstrained, because each legitimately has more than one shape — `Evidence` now points
  at a block, a page or a cell. Those three could take a set-of-alternatives rule instead
  of being exempt.
- Cell nodes come from re-parsing the table HTML, so a table the recognition model got
  structurally wrong yields confidently wrong cells, and an assertion citing one cites a
  cell that is wrong in the same way. Nothing cross-checks the parse against the block's
  plain-text projection. The parse itself is at least *one* parse —
  `vs_extract::tables::parse_html_table`, shared with result-table mining — so the cell a
  citation names and the cell the graph holds cannot disagree.

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
