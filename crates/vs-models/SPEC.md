# vs-models — Specification

Everything that talks to a model. Workers never call model servers directly; they go
through this crate.

## Model registry

`config/models.example.toml` lists every model the suite may use:

```toml
[[model]]
id        = "paddleocr-vl-1.5"
revision  = "2026-01"              # pinned weights revision → part of provenance
family    = "paddleocr_vl"         # selects the output parser and native task prompts
task      = ["page_parse", "region_ocr", "table", "formula"]
endpoints = ["http://gpu-pool-a:8000/v1"]   # OpenAI-compatible base URLs (vLLM)
max_concurrency = 256              # in-flight requests per worker towards this model
timeout_ms = 60000
languages = ["*"]                  # informational; routing is decided by vs-adapt scores
served_name = "PaddleOCR-VL"       # optional: name the server knows the model by (default: id)
api_key_env = "SOME_KEY"           # optional: env var with a bearer token (hosted APIs)
dimensions = 1024                  # embedding models: vector size (required for OpenAiEmbedder)
batch_size = 32                    # embedding models: texts per /embeddings request
image_transport = "base64"         # "url" reserved for presigned URLs of very large pages
```

- `id@revision` is the `model_ref` in provenance (`ModelSpec::model_ref`). Changing weights
  requires a new revision.
- Several endpoints per model = client-side round-robin with passive health checks:
  3 consecutive connection errors or HTTP 5xx on one endpoint eject it for 30 s. HTTP 429
  and other 4xx do not count against health (the endpoint answered). If every endpoint is
  ejected, the one whose ejection ends first is used anyway.
- Hosted APIs (assumption A2) are registered the same way with `api_key_env`; off unless
  listed. A missing or empty variable is logged and the request is sent without a token.

## Client

`OpenAiClient` — `POST {endpoint}/chat/completions` with:

- images as `data:image/png;base64,…` content parts (`image_transport = "url"` is accepted
  but currently behaves like base64 — see the TODO in `build_body`);
- `temperature = 0`, `max_tokens` from the step params;
- structured output with `response_format: {type: "json_schema", strict: true}` where the
  step needs JSON (vLLM guided decoding);
- per-model concurrency semaphore (`max_concurrency`) — the main back-pressure knob.
  vLLM's continuous batching does the actual GPU batching, so the worker's job is to keep
  enough requests in flight. `OpenAiClient::in_flight(model)` exposes the current count.

Retries: connection errors and HTTP 429/503 → up to 2 retries (3 attempts) with jittered
exponential backoff (200 ms · 2^attempt, 50–150 %), each attempt on the next healthy
endpoint; then `ModelError` (steps map it to `StepError::Model`). Other HTTP statuses →
no retry. `ModelError::is_retryable` is the single source of truth.

`Embedder` trait — `model_ref()`, `dimensions()`, `embed_texts()`. `OpenAiEmbedder::new(client,
model, batch_size)` implements it over `POST {endpoint}/embeddings` (`encoding_format =
"float"`), splitting the input into `batch_size` chunks (`0` = the registry's `batch_size`,
default 32), restoring input order from `data[].index`, and rejecting vectors whose length
differs from the registry's `dimensions` (`ModelError::Config` if `dimensions` is missing).
Used in Stage 2.

## Prompts

Prompts live in `prompts/<prompt_id>.md` inside this crate, compiled in with
`include_str!`. `prompt_ref = <prompt_id>@<first 8 hex of sha256(prompt text)>`.
Prompts take `{language}` and step-specific variables. The language hint is always
passed to models that accept it.

Generic prompt files: `page_classify`, `figure_describe`, `chart_to_table`, `page_parse`,
`layout_detect`, `region_ocr`, `table_recognize`, `formula_recognize`.

Document-parsing VLMs are trained on fixed task strings; these are compiled in as prompts
too (ids `paddleocr_vl_ocr|table|formula|chart`, `mineru_layout|text|table|formula`,
`dots_ocr_layout_all|layout_only|text`) and selected by `prompts::family_task_prompt(family,
task)`. Prompt resolution order in every step: `params.prompt` (a prompt id, or literal text
→ `prompt_ref = inline@<hash>`) → the family's native task prompt → the generic prompt file.
`prompts::ALL` / `prompts::by_id` list them for `vs-adapt`.

## Output parsers

Each model family emits its own format. `trait OutputParser { fn parse(&self, raw, page,
model_input_size, node_id) -> Result<Vec<BlockDraft>> }`, one implementation per `family`
(`parsers::parser_for`, families in `parsers::FAMILIES`). All parsers:

- strip code fences and end-of-text markers, and locate JSON inside surrounding prose;
- normalise bboxes to `[0,1]`: values ≤ 1 are fractions; otherwise pixels of the *model
  input* size (`CoordScale::Pixels`), or thousandths for families that emit 0–1000
  coordinates (`CoordScale::Thousandths`: MinerU, Logics-Parsing; values > 1000 fall back
  to pixels). Polygons (8 numbers) are reduced to their extent; degenerate or out-of-range
  boxes are clamped and re-ordered, never rejected;
- map family labels to `BlockKind` through `block_kind_from_label`; unknown labels →
  `BlockKind::Other` with their content kept;
- keep tables as HTML (`BlockContent::Table { html }`; Markdown tables are converted,
  bare text is wrapped in a one-cell table) and formulas as LaTeX (`BlockContent::Formula
  { latex }`, `$$`/`\[ \]` delimiters stripped). `text` is the plain-text projection
  (table cells tab-separated, rows newline-separated, entities decoded);
- assign `reading_order` densely from the model's order field when present, else from list
  position; attached captions/footnotes become their own `Caption`/`Footnote` blocks right
  after their parent;
- never drop content silently: output that does not match the family's structured format
  goes through `markdown_fallback` (headings, paragraphs, lists, `<table>`s, `$$` formulas,
  images as separate full-page blocks with confidence 0.5).

| Family | Accepted raw formats |
|---|---|
| `paddleocr_vl` | pipeline JSON (`layout_parsing_result.parsing_res_list[]` with `block_label`, `block_bbox` px, `block_content`, `block_order`) or a bare `[{label,bbox,content}]`; region output is plain text/HTML/LaTeX |
| `mineru` | `content_list` JSON (`type`, `bbox` ‰, `text`, `text_level`, `table_body`, `*_caption`, `*_footnote`, `list_items`) or the VLM's tagged output `<\|box_start\|>…<\|box_end\|><\|ref_start\|>type<\|ref_end\|><\|md_start\|>…<\|md_end\|>` |
| `dots_ocr` | `[{bbox px, category, text}]`; `Table` text is HTML, `Formula` text is LaTeX, `Picture` has no text |
| `logics_parsing` | HTML with `<div class="<category>" data-bbox="x0 y0 x1 y1">` per block (‰); nested `<hN>` gives heading levels, `<img alt>` is kept for figures |
| `generic_json` | our schema `{"blocks":[{kind\|label, bbox, text, html?, latex?, level?, confidence?, reading_order?}]}` (bare array or `regions` list accepted) — used with guided decoding on general VLMs |

Parser tests use captured-style raw outputs in `tests/fixtures/<family>/`.

## Model-backed steps

| Kind | Signature | Notes |
|------|-----------|-------|
| `page.parse_e2e` | `page_image → blocks` | One call; parser by family. `generic_json` models get the `page_parse` prompt with a JSON schema. |
| `layout.detect` | `page_image → layout` | `generic_json` models: `layout_detect` prompt + schema `{regions:[{label,bbox,reading_order,score}]}`. Doc-VLMs (`mineru`, `dots_ocr`, …): native layout prompt, family parser, blocks reduced to regions. |
| `region.ocr` | `page_image, layout → blocks` | Decodes the page once, crops each text-like region (+4 px, `params.crop_padding_px`) as PNG, one request per region run concurrently up to the model's `max_concurrency`. `params.kinds` replaces the default target kinds. |
| `table.recognize` | `page_image, layout → blocks` | Same machinery for `table` regions → HTML. |
| `formula.recognize` | `page_image, layout → blocks` | Same machinery for `formula` regions → LaTeX. |
| `figure.describe` | `figure_crops → blocks` | General VLM; JSON output `{description, figure_type, text_in_image, entities[], relations[]}`; prompt asks for factual description only. `BlockContent::Figure.description` holds the whole JSON when `entities`/`relations` are non-empty (Stage 2 reads them, vs-extract `visual.rs`), else the plain description. |
| `chart.to_table` | `figure_crops → blocks` | Only crops whose region kind is `chart` (`params.kinds` may widen). JSON `{chart_type, title, x_axis, y_axis, table_html, description}` → `BlockContent::Chart { crop_uri, data_html, description }`; `text` = title, description and the table's text projection. |

Region steps output one `BlockDraft` per region with the region's `kind`, `bbox`,
`reading_order` and `score` as confidence. Structured region output (JSON / tagged) is
parsed by the family parser and reduced to the region's kind; plain output is taken as
text / HTML / LaTeX. A failed model call fails the step (`StepError::Model`); the DAG's
`optional` flag decides whether that fails the page.

Common params: `model` (registry id, required), `prompt` (prompt id or literal text, optional
override), `max_tokens`, `kinds` (block kind names), `crop_padding_px`, `crop_dpi` (re-render
region at higher DPI from the original — post-v1, uses the page image in v1).

## Tier 1 page classifier

`VlmPageClassifier` implements `vs_classify::VisionClassifier` with the
`page_classify` prompt and a JSON schema matching `VisionSignals`.

## Metrics

Emitted as `tracing` events under target `vs_models::metrics` (the telemetry layer turns them
into Prometheus series):

- `model_request` — fields `model`, `status` (`ok`, HTTP status or `connection_error`),
  `latency_ms`, `prompt_tokens`, `completion_tokens`, `attempts`, `endpoint`
  → `model_requests_total{model,status}`, `model_latency_ms{model}`, `model_tokens_total{model,dir}`;
- `model_in_flight` — `model`, `in_flight` → `model_in_flight{model}`;
- `endpoint_ejected` — `model`, `endpoint`, `failures`, `eject_secs`;
- `parser_failure` — `family`, `step`, `error` → `parser_failures_total{family}`.

Each attempt runs inside a `model_call` span (`model`, `endpoint`, `path`, `attempt`); steps
emit a `model_step` event (`step`, `model`, `node`, region/crop counts).
