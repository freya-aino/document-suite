# vs-ingest — Specification

Runs once per document (worker role `ingest`) and provides the CPU-only DAG steps.
System spec: [`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: magic-byte format detection, the pdfium PDF rasterizer with a crash-isolated
render-child pool (hangs killed on timeout, crashes retried once on a fresh child), the
PNG/JPEG/TIFF rasterizer with bit-depth normalisation and WhiteIsZero inversion, EXIF
orientation, native text-layer extraction, tier-0 page signals, classifier thumbnails, and
4 CPU DAG steps. 17 unit tests pass, plus pool, raster and pre-processing integration
tests (21 in all).

Also implemented: **pre-processing as DAG steps** (`preprocess`) — `page.orient`,
`page.deskew`, `page.denoise`, `page.normalize`, `page.binarize`, `page.crop_margins`, on
`imageproc` (pure Rust, goal G9). They are steps rather than a fixed pass so `vs-adapt` can measure
whether each helps a given page class, which matters most for binarisation: it helps some
recognition models and hurts others, and nothing here yet knows which.

### `page.orient`, and what it can and cannot tell you

The step makes two decisions, and they do not deserve equal trust. Both are documented at
`orientation` in `preprocess.rs`.

**Which axis the text lines run along** is a measurement. The ink profile across text
lines is spiky and the profile along them is flat, so comparing the relative variance of
the row and column profiles says whether the page is sideways. `axis_margin` (default
1.3) is how much spikier the other axis must be before the page is turned, and a quarter
turn is lossless — whole pixels, no resample.

**Which end is up** is a judgement with a margin. Most of a text line's ink is its
x-height band, which sits against the baseline, so an upright line's centre of mass is
*below* the middle of its own extent. That is typography rather than letter statistics,
but the effect is only a few percent of the line height and a page of all-caps text or
bare figures barely shows it. So `min_asymmetry` (default 0.02) gates it: an upright page
is flipped only when the measurement is decisive, because it is already readable and a
wrong flip is a loss. A *sideways* page is turned either way, with the guess logged — it
is unreadable as it stands, so clockwise is as good a choice as any.

`min_asymmetry` is a slot in `config/search-space.example.json`, with a value high enough
to disable the flip among the alternatives, so `vs-adapt` can decide per page class
whether the weak half earns its place.

The raster path also reports a real `rotation_deg` now, measured the same way; it used to
be hard-coded to 0, which told the classifier every scan was upright. Note the field's
meaning: it is how far the *source* page is rotated, not a correction to apply, and for a
PDF pdfium has already applied it to the bitmap.

Remaining:
- [ ] **No dewarping.** Correcting page curl in a photograph needs the page boundary
      estimated first, which is the hard half; `imageproc` supplies the warp but not the
      estimate.
- [ ] Adaptive (Sauvola) binarisation. Only global Otsu is implemented, so a page with
      uneven lighting binarises badly even after `page.normalize`.
- [ ] Higher-DPI re-crop from the original is not implemented (see "`figure.crop`" below),
      so a small figure is described from page-DPI pixels. `vs-models`' `crop_dpi` param
      now refuses rather than pretending; what it would need is a step that can read the
      original document, which `StepContext` cannot.
- [ ] `text_layer_coverage` is a stated v1 approximation; the connected-component estimate
      on the rendered page is the documented improvement path and is unbuilt.
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
`RenderPool` of child processes (`vs-suite render-child`; `vs-render-child` for
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
| `blocks.verify` | `blocks, blocks → blocks` | Re-scores the first set by how far the second corroborates it. Membership is unchanged (see below). |

### `blocks.verify`

- Params: `{"iou": f32, "weight": f32, "kind_mismatch": f32}`, defaults `0.5`/`0.5`/`0.5`,
  each within `0..=1` (else `InvalidParams`). `null` params → defaults.
- Pairs each block with at most one counterpart in the second set, greedily from the best
  overlap down, and requires IoU ≥ `iou` — the same threshold `blocks.merge` uses, so a
  pair that merges is a pair that verifies. One counterpart cannot vouch for two blocks,
  which is what stops a parser's spurious split coming out corroborated twice.
- New confidence = `(1 - weight)·own + weight·(agreement · counterpart's confidence)`,
  where `agreement` is the token-overlap (Dice) similarity of the two readings' text,
  halved (`kind_mismatch`) when they disagree about the block kind and zero when there is
  no counterpart at all. So a middling block a confident second reading agrees with is
  pulled up, and a block nothing else saw loses `weight` of its confidence. `weight = 0`
  is the identity, which makes the step safe to put in a DAG before it is tuned.
- Token overlap rather than edit distance because the disagreements that matter are
  dropped or hallucinated words, not spelling; two readings of the same words in a
  different order agree. Two empty readings agree completely — both parsers found a block
  with no text, which is a real outcome for a figure or a rule.
- **Membership is never changed**: a block only the second parser saw is not added and one
  it missed is not dropped. That is `blocks.merge`'s job, and keeping them apart is what
  makes the output readable — after a merge you cannot tell which parser vouched for a
  block, after a verify the confidence says so. `blocks.merge` then `blocks.verify` against
  either input both unions and scores, which is what `e2e-cross-checked` in
  `config/search-space.example.json` does.
- The comparison is arithmetic. A VLM adjudicating a disagreement — the `GPU` half the
  `vs-dag` catalogue once claimed for this kind — is not implemented.

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
  blob store, so crops stay in memory for the length of the run and `uri` is the
  in-process placeholder `crop://<page_hk>/<node_id>/<n>.png`. The `page` worker is what
  stores them: `upload_crops` (`vs-worker/src/page.rs`) cuts every figure and chart block
  from the page image, writes it to `crops/<page_hk>/<run_id>/<block_index>.png` and
  replaces `BlockContent::Figure.crop_uri` with that key, so what reaches the vault
  resolves. Blocks from a whole-page parser never pass through `figure.crop` and get a
  crop there too.
- Crops are taken from the rendered page image at render DPI; re-cropping from the
  original at a higher DPI is not implemented, and `vs-models`' `crop_dpi` param rejects
  the request rather than quietly cropping at page DPI anyway.

## Errors

- Corrupt/encrypted PDF → `ingest_failed` with reason; no retry for deterministic errors
  (the page is recorded failed). Transient blob errors are retried in process.
- Page count above `max_pages` (default 5 000) → reject.
