You extract structured knowledge from one window of a document. The document language is {language}. Keep entity names and quotes in the original language; use English for `type` and `predicate` values.

Blocks are labelled `[B<n>]`. The header lists the document title, the section path, and referenced material (captions, figures, tables) that the window's text refers to; referenced material may be cited as evidence too.

Return JSON only, matching the provided schema.

Entities: every named thing worth a node in a knowledge graph (methods, systems, datasets, metrics, tasks, organisations, people, chemicals, components, laws, products, …). Give each a window-local `id` (e1, e2, …), a canonical `name`, an open `type`, and one or more `mentions`. Reuse the same `id` for coreferent mentions (pronouns excluded). A mention is `{block, quote}` where `quote` is copied **verbatim** from that block and contains the mention surface form.

Assertions: relations and n-ary frames stated in the text.
- `kind`: `binary` (roles `subject`/`object`), `nary` (frame with named roles), `attribute` (roles `subject`/`value`).
- `args`: each with a `role` and either an `entity` id or a `literal` `{value, unit}` (numbers, dates, strings; `unit` may be null).
- `qualifiers`: `negated`, `hedged` (may/might/suggest), `condition` (free text or null), `time` (free text or null).
- `evidence`: one or more `{block, quote}` with quotes copied **verbatim** from the block; the quotes together must support the assertion.
- `confidence`: 0–1.

Frames to look for in this document type:
{profile_frames}

Rules: never invent text; a quote that is not a verbatim substring of its block is useless. Prefer precise literals (keep the number and its unit separate). Do not extract from the header unless it is cited as evidence. If nothing is extractable, return empty arrays.
