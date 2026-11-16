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

Remaining:
- [ ] **No tests at all** (237 lines, zero `#[test]`). Highest priority in this crate.
- [ ] `EdgeKind::Cites` is declared and documented but never emitted, although
      `vs-extract/src/bib.rs` already produces a `cites` predicate. Wire it up.
- [ ] `EdgeKind::SameAs` is unreachable: entity resolution is always eager, so the
      "resolution deferred" case never happens. Emit it or delete the variant.
- [ ] `validate()` does not enforce the invariants below that this spec claims: the
      "≥ 1 `arg` edge or literal" rule is unchecked, edge endpoint *kinds* are unchecked,
      and the dangling-edge check uses a kind-agnostic key set while uniqueness is
      per-kind — so a same-key node of another kind satisfies an edge.
- [ ] Figure and chart blocks get no embed target, contrary to "Embedding targets" below:
      emission requires non-empty `block.text`, and a figure's text is usually empty while
      its description sits in `BlockContent::Figure`.
- [ ] Intra-page relationships are absent — regions carry only a linear `reading_order`.
      Containment, column flow, form key/value, table cell structure and in-image text all
      need new node and edge kinds.
- [ ] `header_repeated` on a `TableContinues` stitch link is dropped when the link becomes
      an edge; only `confidence` survives.

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

Stage 2 embeds them via `vs-models::Embedder`; the vector store persists them under these
same node keys, so a vector hit resolves straight back to a graph node.

## Invariants (checked by `DocGraph::validate`)

- every edge endpoint exists;
- every `assertion` has ≥ 1 `evidence` edge and ≥ 1 `arg` edge or literal;
- every `mentions` edge points from a block that exists in the same run;
- node keys are unique per kind.
