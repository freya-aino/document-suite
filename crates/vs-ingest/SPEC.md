# vs-ingest — Specification

Runs once per document (worker role `ingest`) and provides the CPU-only DAG steps.
System spec: [`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: magic-byte format detection, the pdfium PDF rasterizer with a crash-isolated
render-child pool (hangs killed on timeout, crashes retried once on a fresh child), the
PNG/JPEG/TIFF rasterizer with bit-depth normalisation and WhiteIsZero inversion, EXIF
orientation, native text-layer extraction, tier-0 page signals, classifier thumbnails, and
3 CPU DAG steps. 15 unit tests pass, plus pool and raster integration tests.

**There is no image pre-processing of any kind.** No deskew, no denoise, no binarization,
no contrast normalisation, no dewarping, no border removal, and no pixel rotation — the
page goes to the model exactly as rendered. `rotation_deg` is computed and then ignored.

Remaining:
- [ ] Pre-processing as DAG steps (`page.orient`, `page.deskew`, `page.denoise`,
      `page.normalize`, `page.binarize`, `page.dewarp`, `page.crop_margins`), so
      `vs-adapt` can score whether each one helps a given page class rather than assuming
      it. Needs `imageproc`; the `image` crate is configured with `png`/`jpeg`/`tiff` only
      and no filter features.
- [ ] **Raster inputs get fabricated signals.** The non-PDF path returns hard-coded
      constants (`text_chars: 0`, `coverage: 0.0`, `image_count: 1`,
      `image_area_ratio: 1.0`, `rotation_deg: 0`), so every scan and photo reaches the
      classifier with no real tier-0 evidence and is judged from its thumbnail alone.
      At minimum: detected orientation and an ink-coverage estimate.
- [ ] Higher-DPI re-crop from the original is not implemented (see "`figure.crop`" below),
      so a small figure is described from page-DPI pixels.
- [ ] `text_layer_coverage` is a stated v1 approximation; the connected-component estimate
      on the rendered page is the documented improvement path and is unbuilt.
- [ ] Crops stay in memory with a placeholder `crop://…` URI because `StepContext` has no
      blob store. Nothing uploads them, so the URIs never resolve.
- [ ] Office formats via a LibreOffice-headless converter pool (A18, post-v1).

## Assumptions this component rests on

| ID | Assumption |
|----|-----------|
| A18 | v1 formats: PDF, PNG, JPEG, TIFF (multi-page). Office formats via an optional converter in ingest. |

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
| `figure.crop` | `page_image, layout → figure_crops` | Crops `figure`/`chart` regions with `padding` (default 2 %), returns crops (see below). |
| `blocks.merge_native` | `text_layer, layout, blocks → blocks` | Assigns native text spans to layout regions (span centre inside region); text regions get native text; regions already covered by recognised blocks (tables, formulas) take those. Spans outside every region become `other` blocks. Confidence: 0.99 for native text. |
| `blocks.merge` | `blocks, blocks → blocks` | Union of two block sets; overlapping blocks (IoU ≥ 0.5, same kind family) keep the higher-confidence one. |

### `figure.crop`

- Params: `{"padding": f32}`; normalised page fraction added on every side, default
  `0.02`, must be within `0..=0.5` (else `InvalidParams`). `null` params → defaults.
- Decodes the page image bytes (PNG/JPEG via the `image` crate), selects `Figure` and
  `Chart` layout regions in reading order, expands each bbox by `padding` clamped to
  `[0, 1]`, crops (pixel edges rounded) and encodes each crop as PNG. Regions that
  collapse to zero pixels are skipped. Decoding/encoding runs on a blocking thread.
- Output `FigureCrop { region, uri, bytes }`: `region` keeps the source kind, score and
  reading order but its `bbox` is the **padded** crop area, i.e. the exact page area the
  crop bytes show (the unpadded bbox is the matching layout region). `StepContext` has no
  blob store, so crops stay in memory; `uri` is the placeholder
  `crop://<page_hk>/<node_id>/<n>.png` for the component that uploads them (the worker
  or a following step), which also sets `BlockContent::Figure.crop_uri`.
- Crops are taken from the rendered page image at render DPI; re-cropping from the
  original at a higher DPI is not implemented.

## Errors

- Corrupt/encrypted PDF → `ingest_failed` with reason; no retry for deterministic errors
  (`term` + DLQ). Transient blob errors → `nak` with backoff.
- Page count above `max_pages` (default 5 000) → reject.
