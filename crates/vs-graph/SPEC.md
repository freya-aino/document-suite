# vs-graph — Specification

Builds the **document graph**: one in-memory, store-independent graph per document run,
combining the structure layer (from stitching) and the semantic layer (from extraction).
`vs-helix` and `vs-vault` both write from this one structure, so the two stores never
disagree about what was extracted.

## Node kinds

| Kind | Key | Properties |
|------|-----|-----------|
| `document` | `document_hk` | title, language, page count, doc types, run id |
| `page` | `page_hk` | page no, profile, dag ref |
| `section` | heading `block_hk` | title, level |
| `block` | `block_hk` | kind, text, bbox, page no, global order, confidence, (table html / latex / figure description) |
| `entity` | `entity_hk` (canonical) or doc-local id before resolution | name, type id, raw type, aliases |
| `assertion` | `assertion_hk` | predicate id, raw predicate, kind, qualifiers, confidence, verified |

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
| `evidence` | assertion → block (props: char span) | link |
| `related` | entity → entity (props: predicate id, assertion_hk, confidence) | semantic shortcut |
| `cites` | block → entity (reference entry) | semantic |
| `same_as` | doc-local entity → canonical entity (only if resolution deferred) | semantic |

Literal arguments (numbers, dates) are stored as properties on the `arg` list of the
assertion node (`literals: [{role, value, unit}]`), not as nodes.

## Embedding targets

The builder emits `EmbedTarget { node_key, kind, text }` for:
- body blocks with text (paragraphs, lists, captions; tables as linearised text),
- figure/chart blocks (description + in-image text; image embeddings post-v1),
- entities (name + type + one context sentence),
- sections (title + first paragraph) for coarse retrieval.

Stage 2 embeds them via `vs-models::Embedder`; vectors are written by `vs-helix`.

## Invariants (checked by `DocGraph::validate`)

- every edge endpoint exists;
- every `assertion` has ≥ 1 `evidence` edge and ≥ 1 `arg` edge or literal;
- every `mentions` edge points from a block that exists in the same run;
- node keys are unique per kind.
