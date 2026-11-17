# vs-classify — Specification

Turns a page into a `PageProfile`, the only page information the router uses. It must be
**cheap**: classification cost is paid on every page, whatever DAG runs afterwards.
System spec: [`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: tier 0 rules, the tier 1 `VisionClassifier` trait (`VlmPageClassifier` in
`vs-models`), `combine()`, and **document-level classification** — the weighted page-vote
aggregation in `document`, the `DocumentClassifier` trait (`VlmDocumentClassifier` in
`vs-models`, one call over the opening thumbnails), and `resolve()`, which prefers the
most trustworthy source available. 15 unit tests. Pure functions, no I/O.

Remaining:
- [ ] **No automatic cross-page smoothing, and it is not a small omission to fill.**
      `known_doc_type` now comes from the caller's hint, which is the only source that
      exists before a page is routed. Pages fan out as independent tasks across workers
      with no shared per-document state, so page 7 cannot learn what page 1 concluded
      without a round trip that would serialise the fan-out — the thing Stage 1 exists to
      do in parallel. The document type is therefore settled in Stage 2, too late to
      affect routing. Closing this properly means either a per-document cache the `page`
      role writes and reads (racy, and only helps later pages) or a cheap first-page pass
      during ingest (which calls no model today).
- [ ] The document classifier's accuracy is unmeasured; `vs-adapt` scores DAG output, not
      classifier verdicts.
- [ ] Tier 0's measured signals for a scan are `ink_coverage` and `rotation_deg`, which
      are enough to skip a blank page and to notice a sideways one, but not to tell a
      photograph from a form. Everything else about a raster page still comes from the
      tier-1 thumbnail.
- [ ] `confidence` is never calibrated against the labelled datasets `vs-adapt` runs, so
      `min_classifier_confidence` in the routing table is tuned blind.
- [ ] Language/script detection: the caller's hint is always trusted (assumption A19).
- [ ] Nothing *routes* on `rotation_deg`. The signal is now real for rasters as well as
      PDFs, and `page.orient` turns the page when a DAG includes it, but no routing rule
      selects a DAG with that step because a page arrived sideways. Note what the field
      means: how far the source page is rotated, not a correction to apply — see
      `PageSignals::rotation_deg`.

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A14 | Page classifier v1 = rules (tier 0) + one small vision classifier (tier 1). |

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
has_images   = image_count > 0 && image_area_ratio >= min_image_area_ratio && !blank
blank        = blank_ink_coverage > 0 && 0 < ink_coverage <= blank_ink_coverage
needs_vision = !((born_digital && !has_images && skip_vision_for_text_only_digital) || blank)
```

`ink_coverage` is the fraction of the page darker than its Otsu threshold, measured by
ingest for raster inputs only. A blank scan is not worth a GPU call. The `0 < ink_coverage`
guard is load-bearing: the field defaults to `0.0`, which means **not measured** (every PDF
page, and any signals file written before the field existed), and "we did not look" must
never be read as "there is nothing there".

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
| `thumbnail_long_side_px` | 768 | — read by **both** the `ingest` role (which renders the thumbnail) and the `page` role (which consumes it), from `[classify]`, so the two cannot disagree |
| `blank_ink_coverage` | 0.002 | ink coverage at or below which a scan counts as blank and tier 1 is skipped; 0 disables |

## Post-v1

Confidence calibration against `vs-adapt`'s labelled runs, and a script-detection model
for documents whose language hint is absent or wrong. Everything else once listed here
(rotation correction, document-level smoothing) is now scheduled — see `## Status`.
