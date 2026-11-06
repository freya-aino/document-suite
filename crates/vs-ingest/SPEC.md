# vs-ingest — Specification

Runs once per document (worker role `ingest`) and provides the CPU-only DAG steps.

## Ingest flow

1. Fetch the original from the blob store; compute `sha256` → `document_hk`.
2. **Detect format** from magic bytes (never from file extension or MIME header):
   PDF `%PDF-`, PNG, JPEG, TIFF (`II*\0` / `MM\0*`). Unknown → job fails with
   `unsupported_format`. Office formats: optional converter (LibreOffice headless in a
   separate pool; post-v1 by default, assumption A18).
3. **Enumerate pages**. Multi-page TIFF → one page per frame; single images → one page.
4. For each page, in parallel up to `ingest_parallelism` (default = vCPUs):
   - extract the **native text layer** (PDF only): spans with text, normalised bbox, font size;
   - compute **`PageSignals`** for the classifier (text chars, coverage, invisible OCR
     layer, embedded image count and area ratio, rotation);
   - **rasterize** at `render_dpi` (default 200; DAG steps can request crops at higher
     effective resolution from the original via `figure.crop`) and a classifier thumbnail;
   - write page image, thumbnail, text layer and signals to the blob store
     (skip if the keys already exist — rendering is reused across runs).
5. Insert `ops.page_tasks` rows and publish one `Task::Page` per page (dedup id
   `<job_id>:page:<page_no>`), then set job state `PagesProcessing` with `pages_total`.

### Rasterizer

`trait Rasterizer { page_count, render(page, dpi) -> PageImage, text_layer(page) -> TextLayer, signals(page) -> PageSignals }`

- PDF: **pdfium** via `pdfium-render`, with `libpdfium.so` shipped in the worker rootfs.
  One pdfium instance per OS thread (pdfium is not thread-safe); rendering runs in
  `spawn_blocking`.
- Images: `image` crate; EXIF orientation applied; DPI from metadata or assumed 300.

### Text coverage

`text_layer_coverage` = area of text span boxes ÷ area of visible text-like content.
v1 approximation: span area ÷ (span area + area of image objects that are full-page or
contain no text) — enough to separate born-digital pages from scans with partial OCR.
Improve with a connected-component estimate on the rendered page if adaptation shows
misrouting.

## CPU steps provided

| Kind | Signature | Behaviour |
|------|-----------|-----------|
| `figure.crop` | `page_image, layout → figure_crops` | Crops `figure`/`chart` regions with `padding` (default 2 %), uploads crops, returns crops. |
| `blocks.merge_native` | `text_layer, layout, blocks → blocks` | Assigns native text spans to layout regions (span centre inside region); text regions get native text; regions already covered by recognised blocks (tables, formulas) take those. Spans outside every region become `other` blocks. Confidence: 0.99 for native text. |
| `blocks.merge` | `blocks, blocks → blocks` | Union of two block sets; overlapping blocks (IoU ≥ 0.5, same kind family) keep the higher-confidence one. |

## Errors

- Corrupt/encrypted PDF → `ingest_failed` with reason; no retry for deterministic errors
  (`term` + DLQ). Transient blob errors → `nak` with backoff.
- Page count above `max_pages` (default 5 000) → reject.
