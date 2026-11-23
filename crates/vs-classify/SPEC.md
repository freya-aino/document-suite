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

**Cross-page smoothing exists**, in the one place it can: the **ingest** stage, which
holds the whole document before the pages fan out, shows the opening thumbnails to the
document classifier once per document when `stage2.doc_classifier_model` is set, and every
page is then classified knowing what document it belongs to. The page stage still cannot
do it — pages fan out with no shared per-document state — and Stage 2 is after routing.

**Tier 0 can tell a form from a photograph**, on a raster. Two measurements beyond ink
coverage: `ruling_lines` (the longest unbroken dark run across a row or column — text
never makes one, because a line of type is dark in short runs with white between the
words) and `tone_continuity` (the mid-grey fraction, which a bimodal document does not
have and a photograph does). A ruled page reads as a form with a table on it and a
continuous-tone one as an image, with no model involved. Both are 0 for a PDF, which means
"not measured" and never "none".

**Confidence is about the document type**, because that is what the routing table
thresholds on. A rules-only page whose type nobody determined reports
`unknown_doc_type_confidence` (0.3, below the default threshold) instead of
`rules_only_confidence` — it used to claim 0.85 for a type of `other` that nothing had
looked for, so every such page sailed past `min_classifier_confidence`, which exists to
catch exactly that page. Measured by `vs-adapt classify-score` at 85 % expected
calibration error before, 30 % after.

Remaining:
- [ ] **Accuracy on real documents is unmeasured.** `vs-adapt classify-score` is the thing
      that measures it — per-class precision and recall, a directional confusion table and
      a calibration curve — and it has only run against the synthetic corpus, where the
      rule tier scores 0 % because it has no document-type opinion at all. That number is
      honest and uninteresting; the interesting one needs labelled real documents and a
      vision model.
- [ ] Nothing *routes* on `rotation_deg` **by default**. The router can match on it now
      (`rotated`, `rotated_deg`) and it is in `vs-adapt`'s class ladder, so the question is
      answerable — but no shipped routing table uses it, because whether a sideways page
      wants a DAG with `page.orient` is exactly the measurement nobody has made.
- [ ] Language/script detection: the caller's hint is always trusted (assumption A19).

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
