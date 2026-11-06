# vs-classify — Specification

Turns a page into a `PageProfile`, the only page information the router uses. It must be
**cheap**: classification cost is paid on every page, whatever DAG runs afterwards.

## Two tiers

| Tier | Where | Inputs | Produces |
|------|-------|--------|----------|
| 0 — rules | CPU, in the worker | `PageSignals` from ingest (text layer stats, embedded image stats, rotation) | `born_digital`, `has_images`, text coverage, rotation, whether tier 1 is needed |
| 1 — vision | GPU, one batched call on a thumbnail | downscaled page image (default long side 768 px) + language hint | `doc_type`, probabilities for handwriting, tables, formulas, charts, images |

Tier 1 is skipped when tier 0 is conclusive and `skip_vision_for_text_only_digital` is on:
born-digital page, no embedded images. Those pages get `doc_type = other` unless the
document-level type is already known from an earlier page (ingest passes it in), and
the flags are `false`. They route to text-layer DAGs, which still run layout detection,
so tables and formulas are still found there.

## Tier 0 rules

```
born_digital = text_chars >= born_digital_min_chars
               && text_layer_coverage >= born_digital_min_coverage
               && (!ocr_text_layer || trust_ocr_layer)
has_images   = image_count > 0 && image_area_ratio >= min_image_area_ratio
needs_vision = !(born_digital && !has_images && skip_vision_for_text_only_digital)
```

`ocr_text_layer` = an invisible text layer over a full-page image (a scan that was
OCR'd before). Its quality is unknown, so by default it is **not** trusted and the page
is re-OCR'd. Set `trust_ocr_layer = true` per tenant to reuse it.

## Tier 1 model

- Any small VLM served via `vs-models` with a fixed prompt and a JSON schema output
  (guided decoding), or a dedicated image classifier exposed with the same interface.
- Output schema: `{doc_type, doc_type_confidence, p_handwriting, p_tables, p_formulas,
  p_charts, p_images}`.
- A flag is `true` when its probability is ≥ `flag_threshold` (default 0.5).
- `vs-adapt` evaluates classifier accuracy per class; a wrong class costs quality or speed,
  so classifier choice is part of adaptation.

## Combining

- `confidence` = tier 1 `doc_type_confidence` × mean certainty of the flags
  (`|p − 0.5| × 2`); tier-0-only pages get `rules_only_confidence` (default 0.85).
- `has_images` = tier 0 OR tier 1 (scans have no embedded-image objects, so tier 1
  catches pictures inside scans).
- Router sends pages below `min_classifier_confidence` to the robust DAG.

## Config (`ClassifyConfig`)

| Key | Default |
|-----|---------|
| `born_digital_min_chars` | 50 |
| `born_digital_min_coverage` | 0.8 |
| `min_image_area_ratio` | 0.02 |
| `trust_ocr_layer` | false |
| `skip_vision_for_text_only_digital` | true |
| `flag_threshold` | 0.5 |
| `rules_only_confidence` | 0.85 |
| `thumbnail_long_side_px` | 768 |

## Post-v1

Language/script detection (replacing the hint as an option), rotation correction model,
document-level type smoothing across pages.
