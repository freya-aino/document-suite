# vs-models — Specification

Everything that talks to a model. Workers never call model servers directly; they go
through this crate.

## Model registry

`config/models.example.toml` lists every model the suite may use:

```toml
[[model]]
id        = "paddleocr-vl-1.5"
revision  = "2026-01"              # pinned weights revision → part of provenance
family    = "paddleocr_vl"         # selects the output parser
task      = ["page_parse", "region_ocr", "table", "formula"]
endpoints = ["http://gpu-pool-a:8000/v1"]   # OpenAI-compatible base URLs (vLLM)
max_concurrency = 256              # in-flight requests per worker towards this model
timeout_ms = 60000
languages = ["*"]                  # informational; routing is decided by vs-adapt scores
```

- `id@revision` is the `model_ref` in provenance. Changing weights requires a new revision.
- Several endpoints per model = client-side round-robin with health checks (passive:
  3 consecutive failures → endpoint ejected for 30 s).
- Hosted APIs (assumption A2) are registered the same way with `api_key_env`; off unless
  listed.

## Client

`OpenAiClient` — `POST {endpoint}/chat/completions` with:

- images as `data:image/png;base64,…` content parts (or a presigned URL if
  `image_transport = "url"` for very large pages);
- `temperature = 0`, `max_tokens` from the step params;
- structured output with `response_format: {type: "json_schema", …}` where the step needs JSON
  (vLLM guided decoding);
- per-model concurrency semaphore (`max_concurrency`) — the main back-pressure knob.
  vLLM's continuous batching does the actual GPU batching, so the worker's job is to keep
  enough requests in flight.

Retries: connection errors and HTTP 429/503 → up to 2 retries with jittered backoff,
then `StepError::Model`. HTTP 4xx (except 429) → no retry.

`Embedder` trait — `POST {endpoint}/embeddings`; batch size from config; used in Stage 2.

## Prompts

Prompts live in `prompts/<prompt_id>.md` inside this crate, compiled in with
`include_str!`. `prompt_ref = <prompt_id>@<first 8 hex of sha256(prompt text)>`.
Prompts take `{language}` and step-specific variables. The language hint is always
passed to models that accept it.

## Output parsers

Each model family emits its own format (Markdown with tags, HTML with bbox attributes,
JSON, DocTags, …). `trait OutputParser { fn parse(&self, raw: &str, page: &PageImage) -> Result<Vec<BlockDraft>> }`,
one implementation per `family`. Parsers must:

- normalise bboxes to `[0,1]` (families report pixels of the *model input* size, which can
  differ from the rendered size — the parser receives both sizes);
- map family labels to `BlockKind`;
- keep tables as HTML and formulas as LaTeX;
- never drop content silently: unknown labels → `BlockKind::Other`.

Parser unit tests use captured raw outputs in `tests/fixtures/<family>/`.

## Model-backed steps

| Kind | Signature | Notes |
|------|-----------|-------|
| `page.parse_e2e` | `page_image → blocks` | One call; parser by family. |
| `layout.detect` | `page_image → layout` | Layout model; regions + reading order. |
| `region.ocr` | `page_image, layout → blocks` | Crops text/handwriting regions, one request per region (batched by vLLM). `params.kinds` filters which region kinds. |
| `table.recognize` | `page_image, layout → blocks` | Table regions → HTML. |
| `formula.recognize` | `page_image, layout → blocks` | Formula regions → LaTeX. |
| `figure.describe` | `figure_crops → blocks` | General VLM; JSON output `{description, entities[], text_in_image}`; prompt asks for factual description only. |
| `chart.to_table` | `figure_crops → blocks` | Chart crops → data table HTML + short description. |

Common params: `model` (registry id, required), `prompt` (prompt id, optional override),
`max_tokens`, `crop_dpi` (re-render region at higher DPI from the original — post-v1,
uses the page image in v1).

## Tier 1 page classifier

`VlmPageClassifier` implements `vs_classify::VisionClassifier` with the
`page_classify` prompt and a JSON schema matching `VisionSignals`.

## Metrics

`model_requests_total{model,status}`, `model_latency_ms{model}`,
`model_tokens_total{model,dir}`, `model_in_flight{model}`, `parser_failures_total{family}`.
