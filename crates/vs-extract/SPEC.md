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
| `Assertion` | `local_id`, `predicate` (raw), `predicate_id` (normalised), `kind` (`binary`/`nary`/`attribute`), `args: [{role, value}]` where value = `Entity{local_id}` or `Literal{value, unit}`, `qualifiers` (`negated`, `hedged`, `condition`, `time`), `evidence: [Evidence]`, `confidence`, `verified`, `provenance_idx` |
| `ExtractionOutput` | all of the above + `provenance` (one entry per model call or CPU step) + `dropped_ungrounded`, `dropped_unverified` |
| `Fragment` | `mentions`, `entities`, `assertions` — a partial result from one source before merging |

Binary relations are assertions with roles `subject` / `object`. N-ary frames use named
roles (see frames). This one shape is stored the same way in the vault and the graph.

Local ids are prefixed by their source so they are unique across a document:
`w<i>.e1` / `w<i>.m1` / `w<i>.a1` (window `i`), `tbl<block>.…`, `bib.r1` / `bib.c1`,
`fig<block>.…`.

## Pipeline (`LlmExtractor::extract`)

1. **Windows** (`build_windows`, `render_window`): running-text body blocks (not tables,
   formulas, headings, furniture or reference-list blocks) grouped by section, packed up to
   `window_chars` (default 12 000), never splitting a block; a block larger than the limit is
   its own window. Paragraphs linked by `ContinuesOn` stay together. Blocks before the first
   heading form a front-matter window. `render_window` produces the prompt text: a header
   (document title, section path, and the text of tables/figures/captions **referred to**
   from inside the window via `RefersTo`/`CaptionOf` links, each labelled too) and the body
   with `[B<n>]` labels; `RenderedWindow::labels` maps label → `block_hk`.
2. **Extraction call** (one per window, `max_concurrency` in flight, default 8; prompt
   `prompts/extract.md` with `{language}` and `{profile_frames}` from `DocProfile`; JSON
   schema `schemas/extraction.schema.json`, strict-mode compatible). The model returns
   `entities[{id, name, type, mentions[{block, quote}]}]` and
   `assertions[{predicate, kind, args[{role, entity|literal{value, unit}}], qualifiers,
   evidence[{block, quote}], confidence}]`. Parsing (`parse::parse_raw`) tolerates code
   fences and missing optional fields.
3. **Grounding** (`parse::ground_output` over `locate_span`): each quote is located in its
   block's `text` → char span (exact, then case-insensitive with whitespace collapsed).
   Mentions are narrowed to the entity name inside the quote. Not found → the mention /
   evidence item is dropped; an entity without grounded mentions, an assertion without
   grounded evidence, or an assertion referencing an unknown entity is **dropped**
   (`drop_ungrounded = true`, default) and counted in `dropped_ungrounded`. With
   `drop_ungrounded = false` unlocated mentions are kept with span `0..0` and
   evidence-less assertions survive.
4. **CPU sources** (each gets its own `Provenance` with `model_ref = None`):
   - **Tables** (`tables::extract_tables`, step `stage2.tables`): `<tr>/<td>/<th>` scan of
     table HTML (also chart `data_html`). When the header row has a metric-like column
     (accuracy, F1, BLEU, `%`, …) and at least half of those cells are numeric while row
     labels are not, each cell becomes a `result` frame (`method` = row-label entity,
     `metric` = header entity, `value` = literal with unit; caption as `condition`),
     evidence = span of the cell text in the table block's plain text, confidence 0.6.
   - **Bibliography and citations** (`bib::extract_bibliography`, step
     `stage2.bibliography`): reference-list blocks are those in a section titled
     References / Bibliography / Literatur(verzeichnis) / Works cited / …, blocks of
     `BlockKind::Reference`, or blocks starting with `[n]` that contain a year. Entries are
     split at `[n]` markers or, unmarked, merged by lines until a chunk has a year and the
     next line looks like an entry start. Each entry → a `reference` entity (name = title
     heuristic; aliases `[n]`, `Surname et al., year`, `Surname (year)`, `doi:…`).
     Citation markers in body blocks — `[12]`, `[3,4]`, `[1-3]`, `(Smith et al., 2024;
     Doe 2019)`, `Smith et al. (2024)` — yield `cites` frames (kind `nary`): `claim_block`
     = literal holding the citing `block_hk`, `reference` = entity; evidence = span of the
     marker; confidence 0.9 (numeric) / 0.8 (author-year).
   - **Figures** (`visual::extract_figures`, step `stage2.figures`): a figure/chart whose
     `description` is the `figure.describe` JSON (`entities`, `relations`) yields entities
     and binary assertions with `Evidence::Region` at the figure bbox, confidence 0.7.
5. **Visual evidence** (`visual::regionalise`): assertions the LLM grounded in a
   figure/chart block's description text get `Evidence::Region` (page, bbox) instead of the
   span and confidence capped at 0.7 (spec step 8).
6. **Verification** (profiles `quality` and `balanced`, configurable via
   `verify_profiles`; off for `fast`): LLM-extracted assertions with span evidence are
   checked in batches of `verify_batch` (default 10) with `prompts/verify.md` and
   `schemas/verify.schema.json` (`{verdicts:[{id, supported}]}`), given the assertion in
   compact form and its evidence text. Unsupported → dropped (`dropped_unverified`);
   supported → `verified = true`; no verdict → kept unverified. Step `stage2.verify`.
7. **In-document resolution** (`resolve::resolve_in_document`): entities are merged when
   their normalised names match (lowercase, whitespace collapsed, surrounding punctuation
   and a leading article removed) and their raw types are compatible (equal slugs, or one is
   generic/empty). Abbreviation definitions `Long Form (LF)` found anywhere in the body join
   both forms; the long form becomes the name. Other member names and all aliases are kept as
   `aliases` (case-insensitive dedupe); mentions are concatenated; assertion arguments are
   rewritten to the surviving `local_id`.
8. **Type normalisation** (`TypeNormalizer`): each distinct raw entity type → `type_id`, each
   distinct predicate (with the roles seen for it) → `predicate_id`.
9. **Corpus resolution** (`CorpusResolver`, implemented in vs-helix): not called by
   `extract`; the worker runs it afterwards and sets `entity_hk`.

Model or JSON errors fail the whole extraction (`ExtractError::Model` / `Schema`) so the
worker can retry the document.

## Provenance

One `Provenance` per model call: `run_id` from the document, `dag_ref = "stage2@1"`,
`node_id` (`extract.w<i>`, `verify.b<i>`, `tables`, …), `step_kind` (`stage2.extract`,
`stage2.verify`, `stage2.tables`, `stage2.bibliography`, `stage2.figures`), `model_ref` from
the client's registry spec, `prompt_ref = <id>@<sha256 prefix>` of the compiled-in prompt,
`params_hash = H(window header)` (extract) / `H(rendered items)` (verify), `started_at`,
`duration_ms`. Assertions point at it via `provenance_idx`.

## Type normalisation and the ontology

```rust
pub struct OntologyType { pub id: String, pub label: String, pub synonyms: Vec<String>, pub review_status: String }

#[async_trait]
pub trait OntologyStore: Send + Sync {
    async fn entity_types(&self) -> Result<Vec<OntologyType>, ExtractError>;
    async fn relation_types(&self) -> Result<Vec<OntologyType>, ExtractError>;
    async fn insert_pending_entity_type(&self, id: &str, label: &str) -> Result<(), ExtractError>;
    async fn insert_pending_relation_type(&self, id: &str, label: &str, roles: &[String]) -> Result<(), ExtractError>;
}
```

`EmbeddingTypeNormalizer<S: OntologyStore>::new(store, embedder: Arc<dyn vs_models::Embedder>, threshold)`
maps a raw string to the ontology: an exact slug match on id / label / synonym wins without
embedding; otherwise the raw string and every label + synonym are embedded (cached in a
`Mutex<HashMap>`) and the best cosine ≥ `type_match_threshold` (default 0.85) wins. No match →
`insert_pending_*` with `id = slug(raw)`, `label = raw` (review_status `pending`, assumption
A10). Decisions are cached per instance; `rejected` types are never matched.
`InMemoryOntologyStore` is provided for tests and single-process use; the worker implements
the trait over `registry.entity_types` / `registry.relation_types`.

`NoopTypeNormalizer` sets `type_id = slug(raw)` (tests, fast profile).

## Frames (scientific profile, v1)

| Frame | Roles |
|-------|-------|
| `result` | `method`, `task`, `dataset`, `metric`, `value`, `condition?` |
| `uses` | `agent` (method/system), `resource` (dataset/tool/model) |
| `compares` | `subject`, `baseline`, `metric?`, `outcome` (better/worse/equal) |
| `proposes` | `author_or_paper`, `artifact` |
| `cites` | `claim_block`, `reference` (bibliography entry entity) |
| `defines` | `term`, `definition` |

`DocProfile` (`Scientific`, `Legal`, `Financial`, `Generic`) selects the frame list in the
prompt.

## Config and construction

```rust
pub struct ExtractConfig {
    pub extraction_model: String,      // registry id
    pub verify_model: String,          // registry id
    pub window_chars: usize,           // 12_000
    pub drop_ungrounded: bool,         // true
    pub verify_profiles: Vec<Profile>, // [Quality, Balanced]
    pub type_match_threshold: f32,     // 0.85 (for the normalizer the worker builds)
    pub max_concurrency: usize,        // 8
    pub verify_batch: usize,           // 10
    pub max_tokens: u32,               // 4096
    pub language: Option<String>,      // prompt hint; None → "the document's language"
    pub doc_profile: DocProfile,       // Scientific
}
ExtractConfig::new(extraction_model, verify_model)  // defaults for the rest
LlmExtractor::new(client: Arc<OpenAiClient>, config, normalizer: Arc<dyn TypeNormalizer>)
LlmExtractor::with_backend(backend: Arc<dyn ChatBackend>, config, normalizer)  // tests
```

`ChatBackend` abstracts the chat side of `OpenAiClient` (`model_ref(model)`, `chat(model,
req)`) so the orchestration is unit-tested with a fake. Corpus-resolution settings
(`link_threshold`, `judge_band`, `embedding_model`) belong to the resolver (vs-helix).

## Quality targets (to measure in vs-adapt)

Grounding rate ≥ 0.97 of emitted assertions; verified precision on a labelled scientific
set as the main metric; recall reported per frame.
