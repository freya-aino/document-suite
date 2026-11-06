# vs-extract — Specification

Stage 2 information extraction over a `StitchedDocument`: entities, relations (binary
and n-ary), their evidence, and entity resolution. The quality bar is **scientific
papers**: method/task/dataset/metric/result relations, comparisons, citations, with
every assertion grounded in a quoted span.

## Output model

| Type | Fields |
|------|--------|
| `Mention` | `id`, `block_hk`, `char_start`, `char_end`, `surface`, `raw_type`, `confidence` |
| `DocEntity` | `local_id`, `name` (canonical within the doc), `raw_type`, `type_id` (normalised), `aliases`, `mentions`, `entity_hk` (after corpus resolution) |
| `Assertion` | `local_id`, `predicate` (raw), `predicate_id` (normalised), `kind` (`binary`/`nary`/`attribute`), `args: [{role, value}]` where value = entity ref or literal (number + unit, date, string), `qualifiers` (`negated`, `hedged`, `condition`, `time`), `evidence: [Evidence]`, `confidence`, `verified` |
| `ExtractionOutput` | all of the above + `provenance` for each model call |

Binary relations are assertions with roles `subject` / `object`. N-ary frames use named
roles (see frames). This one shape is stored the same way in the vault and the graph.

## Pipeline

1. **Windows** (`build_windows`): body blocks grouped by section, packed up to
   `window_chars` (default 12 000), never splitting a block. Paragraphs linked by
   `ContinuesOn` stay together. Each window carries a header: document title, section
   path, and the text of captions/figure descriptions/tables that are **referred to**
   from inside the window (via stitch links), so relations to figures and tables can be
   extracted. Blocks are labelled `[B<n>]` in the prompt.
2. **Extraction call** (one per window, concurrent; JSON schema in
   `schemas/extraction.schema.json`; guided decoding). The model returns entities and
   assertions; every entity and every assertion cites `block` + exact `quote`.
   The prompt includes the language hint and a doc-type profile (scientific, legal,
   financial, generic) selecting which frames to look for.
3. **Grounding** (`locate_span`): each quote is located in its block's `text` → char span.
   Exact match first, then whitespace/case-insensitive match. Not found → the item is
   **dropped** (`drop_ungrounded = true`, default) and counted in metrics. This is the
   main hallucination guard.
4. **Verification** (profile `quality` and `balanced`; off for `fast`): one batched
   check per assertion: given evidence text, is the assertion supported? Unsupported →
   dropped; supported → `verified = true`.
5. **In-document resolution**: merge entities by normalised name; abbreviation
   definitions (`Long Form (LF)`) join both forms; the extraction call's own coreference
   ids within a window are kept; across windows, same normalised name + compatible type
   → same entity.
6. **Type normalisation** (`TypeNormalizer`): raw entity types and predicates are mapped
   to the ontology in `registry.entity_types` / `registry.relation_types` by embedding
   similarity (≥ `type_match_threshold`, default 0.85) with synonyms. No match → new type
   with `review_status = pending` (assumption A10). The ontology grows from data.
7. **Corpus resolution** (`CorpusResolver`): each `DocEntity` is matched against existing
   canonical entities in HelixDB (vector search on name+type+context embedding, then
   string similarity). Score ≥ `link_threshold` → link; ambiguous band → LLM judge;
   otherwise new canonical entity. Result sets `entity_hk`.
8. **Visual relations**: `figure.describe` entities/relations become mentions/assertions
   with `Evidence::Region` on the figure's bbox (lower base confidence, 0.7).

## Frames (scientific profile, v1)

| Frame | Roles |
|-------|-------|
| `result` | `method`, `task`, `dataset`, `metric`, `value`, `condition?` |
| `uses` | `agent` (method/system), `resource` (dataset/tool/model) |
| `compares` | `subject`, `baseline`, `metric?`, `outcome` (better/worse/equal) |
| `proposes` | `author_or_paper`, `artifact` |
| `cites` | `claim_block`, `reference` (bibliography entry entity) |
| `defines` | `term`, `definition` |

Tables are a first-class source: rows of result tables become `result` frames, with
evidence pointing to the table block (and the cell text as quote).

Bibliography blocks are parsed into `reference` entities (title, authors, year, venue, DOI).
Citation markers in body text (`[12]`, `(Smith et al., 2024)`) link to them.

## Config

`window_chars`, `drop_ungrounded`, `verify` (per profile), `type_match_threshold`,
`link_threshold`, `judge_band` (e.g. 0.7–0.85), `extraction_model`, `verify_model`,
`embedding_model`.

## Quality targets (to measure in vs-adapt)

Grounding rate ≥ 0.97 of emitted assertions; verified precision on a labelled scientific
set as the main metric; recall reported per frame.
