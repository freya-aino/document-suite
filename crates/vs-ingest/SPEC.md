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

`trait Rasterizer { page_count, render(page, dpi), thumbnail(page, long_side), text_layer(page), signals(page), page(page, dpi, long_side) -> PageAssets }`

Pages are 1-based. `page()` returns all of a page's assets in one pass and is what the
ingest flow calls.

- PDF: **pdfium** via `pdfium-render`, bound from `PDFIUM_DYNAMIC_LIB_PATH` (set by
  devenv) or the system library path (`libpdfium.so` in the worker rootfs). pdfium is
  not thread-safe and keeps global state, so a process has one binding and every
  document operation holds a process-wide lock (`pdfium()` returns a guard);
  `pdfium-render`'s per-call locking alone corrupted the heap. PDFs are opened from a
  file (`open_rasterizer_file`, read lazily) or from bytes (tests).
- Boxes (text spans, images) are normalised to the page **as rendered**, i.e. with
  `/Rotate` applied, so they line up with the page image.
- Page images are capped at 12 000 px per side; `PageImage.dpi` is the effective DPI.
- Images: `image` crate (PNG, JPEG with EXIF orientation applied) and the `tiff` crate
  for TIFF (one page per frame; bilevel/CCITT, 8/16-bit gray, RGB, RGBA; WhiteIsZero
  inverted). DPI from the TIFF resolution tags, otherwise assumed 300. Images are
  downscaled to the requested DPI but never upscaled.

### Text coverage

`text_layer_coverage` = area of text span boxes ÷ area of visible text-like content.
v1 approximation: span area ÷ (span area + area of image objects that are full-page or
contain no text) — enough to separate born-digital pages from scans with partial OCR.
Improve with a connected-component estimate on the rendered page if adaptation shows
misrouting.

### Render children (`child`, `pool`)

**pdfium never runs in the worker process.** The ingest role renders through a
`RenderPool` of child processes (`vs-worker render-child`; `vs-render-child` for
tests), one pdfium binding each:

- Stateless protocol: a JSON request line names the file, format and page; the reply is
  a length-prefixed JSON header plus the PNG and JPEG payloads. Any child can serve any
  page, so a document's pages render in parallel (pool size = CPUs by default).
- **Hang:** the parent kills a child that exceeds `page_render_timeout_s` (default
  60 s) → `Timeout`, which fails the document **permanently** (the input is the cause).
- **Crash:** retried once on a fresh child; a second crash → `Crashed`, permanent.
- Each child caps its address space (`render_mem_limit_mb` → `RLIMIT_AS`) and dies
  with its parent (`PR_SET_PDEATHSIG`), so a SIGKILLed worker leaves no renderer behind.
- Child-side errors (corrupt, encrypted, page out of range) come back as errors and
  the child is reused.

### Spooling

The ingest role streams the original from the object store to `scratch_dir` (the
microVM scratch disk), hashing it on the way, and renders from that file; it is
deleted when the task ends. Memory use does not grow with file size: at most one
rendered page per child is held before upload.

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
