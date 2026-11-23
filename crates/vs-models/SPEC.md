# vs-models — Specification

Everything that talks to a model. Workers never call model servers directly; they go
through this crate. System spec: [`docs/SPEC.md`](../../docs/SPEC.md) — decision D3
(OpenAI-compatible access) and D7 (guided decoding).

## Status

Implemented: the model registry with declared capabilities, the OpenAI-compatible client
with round-robin over endpoints, passive health ejection and jittered retries, the
per-model concurrency semaphore, `OpenAiEmbedder`, 8 compiled-in prompts plus the doc-VLM
families' native task prompts, **6 output parsers with captured-output fixtures**
(including `auto`), 7 model-backed steps, the tier-1 page classifier, the `url` image
transport behind an `ImageHost`, and per-model image downscaling. 29 unit tests plus 28
integration tests — the best-covered crate in the workspace.

**A model with no bespoke parser now needs no Rust.** `family = "auto"` plus a few
capability fields in the registry is the whole integration. See "The `auto` family".

**`coord_scale` reaches every family**, not only `auto` and `generic_json`: each family
keeps its documented convention as its default and a registry entry overrides it. The
declaration used to be read, logged about and ignored for the document-VLM families, with
the boxes coming out scaled by the ratio of the page size to 1000 and nothing saying why.

**`crop_dpi` works.** A step asks `StepContext::pages` for the page at a finer DPI and
crops from that, so a figure occupying a tenth of the page is no longer described from a
tenth of the page's pixels. Every way of not getting one — no capability, an original past
its retention, a DPI below the page's — falls back to the page image and logs the reason.

**Which parse path `auto` took is in the node's trace**, through `StepOutput::metrics`,
and `vs-adapt`'s report names it per DAG and calls out pages that fell back to splitting
Markdown. It used to go to a `tracing` target an operator could read and the scorer could
not — so the one question `auto` exists to answer was the one thing nothing could score.

Remaining:
- [ ] The model landscape below is from vendor-curated leaderboard snapshots, not from our
      own measurement. `vs-adapt` is the thing that should decide, and it has only ever
      run on the synthetic corpus `vs-adapt dataset` generates — which measures the
      machinery, not the models.
- [ ] `grounded_tags` is **written from the published output format, not from a capture**.
      No sample from a running model has gone through it. What makes that safe rather than
      reckless is the fallback: output that does not match the format reaches the Markdown
      splitter exactly as it did before the family existed, and `auto`'s own test asserts
      that MinerU's overlapping tag spelling still goes to MinerU. A captured sample from
      each of GLM-OCR, HunyuanOCR, LightOnOCR and DeepSeek-OCR would turn this from
      "reads the documented format" into "reads what the model emits", and needs the
      models.

## The `auto` family

For a model the suite has no parser for. `auto` inspects the output and routes it to
whichever known family can read it, falling back to the Markdown splitter, which never
fails. The result is identical to naming the family outright — that equivalence is
asserted against the captured fixtures in `tests/auto.rs`.

Candidate order is decided by the *shape* of the output, not by a fixed list, and that
matters more than it looks:

| Output looks like | Tried first |
|---|---|
| `<\|box_start\|>` / `<\|md_start\|>` tags | `mineru` |
| HTML with `data-bbox` | `logics_parsing` |
| JSON with `blocks` / `regions` | `generic_json` (unmistakably our schema) |
| JSON with `text_level` / `page_idx` / `table_body` | `mineru` |
| JSON with `parsing_res_list` / `block_label` | `paddleocr_vl` |
| JSON with `category` | `dots_ocr` |
| anything else | the families in turn, then Markdown |

`generic_json` goes **last** among the JSON candidates unless the payload is unmistakably
ours, because it is the most permissive reader here: it accepts any item carrying a `type`
or `label` field. Tried early it would claim MinerU's `content_list` and read its
per-mille boxes as pixels — a plausible-looking result with every box in the wrong place.
It also silently falls back to Markdown when there is no JSON at all, which is why it is
never offered for non-JSON output: it would swallow MinerU's tagged format before MinerU's
own parser saw it.

An empty parse is not treated as a win, so a parser that reads the output and finds
nothing lets the next candidate try.

## Capabilities

Declared in the registry, so integrating a model is configuration. Each is optional and
falls back to what the family implies, which is what existing registry files rely on.

| Key | Unset default | What it changes |
|-----|---------------|-----------------|
| `supports_json_schema` | true for `generic_json` and `auto`, false otherwise | whether `response_format: json_schema` is sent. Document VLMs are trained on fixed task strings that guided decoding fights with, hence the family-dependent default |
| `supports_vision` | true | a step that sends an image refuses the model up front, instead of surfacing whatever the server says about an unexpected content part |
| `max_image_px` | unset | pages whose long side exceeds it are downscaled and re-encoded as PNG before sending. Images already within the limit are not decoded at all |
| `provider` | none | prefix a gateway needs, e.g. `hosted_vllm`, joined to the served name with `/` |
| `coord_scale` | detected | `fractions`, `thousandths` or `pixels`, for `auto` and `generic_json`. An unrecognised value warns and falls back to detection rather than failing startup |

## Model landscape

Which model to serve is ultimately `vs-adapt`'s decision, measured per page class. This
table is only a starting shortlist. **The numbers are from vendor-curated leaderboard
snapshots (OmniDocBench v1.6 full, dated 2026.04–05) and sources disagree with each
other**, so treat the ordering as indicative and the "family" column as the actionable
part.

| Model | Params | OmniDocBench v1.6 (full) | Family to register |
|---|---|---|---|
| MinerU2.5-Pro | 1.2B | 95.69 | `mineru` |
| GLM-OCR | 0.9B | 95.15 | `generic_json` (it fills a schema you supply) or `auto` |
| PaddleOCR-VL-1.5 | 0.9B | 94.87 | `paddleocr_vl` |
| Youtu-Parsing | 2.5B | 93.68 | `auto` |
| ABot-OCR | 2B | 93.3 | `auto` |
| Logics-Parsing-v2 | 4B | 93.27 | `logics_parsing` |
| FireRed-OCR | 2B | 93.2 | `auto` |
| OpenDoc-0.1B | 0.1B | 90.64 | `auto` |
| dots.ocr | 3B | 90.5 | `dots_ocr` |
| DeepSeek-OCR 2 | 3B | 90.17 | `auto` |
| HunyuanOCR | 1B | 89.87 | `auto` |
| Qwen3-VL-2B | 2B | 80.05 | `generic_json` |

Four of the top six are already covered by a bespoke parser. vLLM ships dedicated
architectures for GLM-OCR, HunyuanOCR and LightOnOCR-1B, so those serve behind the
existing client with no transport work — which, with `auto`, makes them a registry entry.

Note what the parameter counts say: the leaders are all **under 1.5B**, and a 0.1B model
scores 90. Throughput (goal G1) and model quality are not in tension here the way they
would be with a general VLM, so the GPU tier is sized for many small models rather than
few large ones.

## Local development

`config/models.litellm.toml` plus `config/litellm.yaml` run the whole model tier behind one
LiteLLM proxy on `:4000`, started with `vs-litellm-up`. The suite talks plain OpenAI to it
and never learns what is behind it, so swapping which weights serve a slot is a change to
`litellm.yaml` alone.

One trap worth stating: `hosted_vllm/` is LiteLLM's *internal* routing prefix and belongs
in its `litellm_params.model`, not in this crate's registry. The suite asks the proxy for
the proxy's own alias. `provider` is for the other case — pointing the suite straight at a
gateway that wants the prefix itself.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A1 | "VLLM" means vision-language models served by **vLLM**. The client is OpenAI-compatible, so other servers (SGLang, LiteLLM, hosted APIs) also work. |
| A2 | Self-hosted open-weight models by default. Hosted APIs can be registered the same way but are off unless listed. |

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
image_transport = "base64"         # "url" uploads and links instead of inlining base64

# Capabilities — all optional; see "Capabilities" above for the defaults each falls back to.
supports_json_schema = true        # does the server honour response_format: json_schema?
supports_vision = true             # refuse image-sending steps up front if false
max_image_px = 2048                # downscale pages whose long side exceeds this
provider = "hosted_vllm"           # routing prefix a gateway needs, joined with "/"
coord_scale = "thousandths"        # bbox convention, for `auto` / `generic_json` only
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

- images as `data:image/png;base64,…` content parts, or, with `image_transport = "url"`,
  links produced by an `ImageHost` — a seam the worker implements over the blob store, so
  this crate needs no dependency on it. Uploads are content-addressed, so the same page
  sent to two models is stored once and a retried task reuses the object. Asking for
  `"url"` with no `ImageHost` configured warns and inlines, because inlining still works;
- images downscaled to `max_image_px` when the model declares one. An image already within
  the limit is not decoded, and a decode or encode failure sends the original rather than
  failing the step;
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
override), `max_tokens`, `kinds` (block kind names), `crop_padding_px`.

`crop_dpi` — re-render the region from the original at a higher DPI — is **rejected** with
`InvalidParams`, not silently ignored as it was. It needs the original document, and a step
cannot reach it: `StepContext` carries no blob store. Crops come from the page image at
`worker.render_dpi`, so raising that is the way to get more pixels today.

## Classifiers

`VlmPageClassifier` implements `vs_classify::VisionClassifier` with the `page_classify`
prompt and a JSON schema matching `VisionSignals`: one thumbnail, one page's verdict.

`VlmDocumentClassifier` implements `vs_classify::DocumentClassifier` with the
`document_classify` prompt: the opening thumbnails (3 by default) in **one** call,
answering `{doc_type, confidence}`.

They are separate models and separate calls because they answer different questions, and
the document one cannot be derived from the page one. A signature page judged on its own
genuinely *is* a `handwritten_note`; judged as page 12 of a contract it is a `contract`.
A vote over per-page verdicts can discount that page but never reinterpret it, which is
why the document classifier is worth its own call rather than more post-processing.

## Metrics

Prometheus series, emitted directly:

| Series | Kind | Notes |
|---|---|---|
| `model_requests_total{model,status}` | counter | one per completed call. `status` is a **bounded** label — `ok`, `client_error`, `server_error`, `connection_error`, `bad_response`, `config`, `unknown_model` — not the error text, which embeds the offending response and would be one series per malformed reply. |
| `model_request_duration_seconds{model}` | histogram | the whole call including retries, because that is what the caller waits for |
| `model_request_attempts{model}` | histogram | 1 on a first-try success; the tail is the retry rate |
| `model_tokens_total{model,kind}` | counter | `kind` is `prompt` or `completion` |
| `model_in_flight{model}` | gauge | against the model's `max_concurrency` |
| `model_endpoint_ejections_total{model,endpoint}` | counter | labelled by endpoint, because ejecting one of four is a capacity question and ejecting all four is an outage |

The richer `tracing` events on target `vs_models::metrics` stay alongside them, carrying
what a label cannot — the endpoint, the exact status line, the error message:

- `model_request` — fields `model`, `status` (`ok`, HTTP status or `connection_error`),
  `latency_ms`, `prompt_tokens`, `completion_tokens`, `attempts`, `endpoint`
  → `model_requests_total{model,status}`, `model_latency_ms{model}`, `model_tokens_total{model,dir}`;
- `model_in_flight` — `model`, `in_flight` → `model_in_flight{model}`;
- `endpoint_ejected` — `model`, `endpoint`, `failures`, `eject_secs`;
- `parser_failure` — `family`, `step`, `error` → `parser_failures_total{family}`.

Each attempt runs inside a `model_call` span (`model`, `endpoint`, `path`, `attempt`); steps
emit a `model_step` event (`step`, `model`, `node`, region/crop counts).
