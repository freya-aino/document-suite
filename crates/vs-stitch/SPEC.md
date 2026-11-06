# vs-stitch — Specification

First step of Stage 2. Turns the independent `PageResult`s of one document into one
`StitchedDocument`. Stage 1 works per page for parallelism; stitching repairs everything
that page boundaries break. It is **CPU-only and deterministic** (no model calls in v1).

## Input / output

- Input: all `PageResult`s of a run (failed pages missing), sorted by `page_no`.
- Output: `StitchedDocument`:
  - `blocks`: every block with `page_no` and a document-wide `global_order`;
  - `furniture`: block keys of running headers/footers/page numbers (kept, but excluded
    from body text and extraction);
  - `sections`: heading tree (`level`, title block, child sections, body block range);
  - `links`: stitching decisions, each with a `LinkKind` and a confidence, so every merge
    is traceable and reversible.

Original blocks are never modified; stitching only adds links. Merged text (e.g. a
paragraph split over two pages) is produced on demand by `StitchedDocument::text_of(chain)`.

## Operations (in order)

1. **Global reading order**: `global_order` = page order, then per-page `reading_order`.
2. **Furniture detection**: `Header`/`Footer`/`PageNumber` blocks, plus blocks whose
   normalised text (digits stripped) repeats at a similar position on ≥ 50 % of pages.
3. **Paragraph continuation** (`LinkKind::ContinuesOn`): last body block on page *n* and
   first body block on page *n+1* are both paragraphs/lists, the first does not end with
   terminal punctuation (`.!?:;` and CJK equivalents), and the second starts with a
   lowercase letter or a continuation character. Confidence 0.9 when both hold, 0.6 when
   only the punctuation test holds.
4. **Table continuation** (`LinkKind::TableContinues`): last body block on page *n* and
   first body block on *n+1* are tables with the same column count (from the first row of
   the HTML). If the second table repeats the header row, the repeat is marked.
5. **Caption pairing** (`LinkKind::CaptionOf`): each caption links to the nearest
   figure/table/chart on the same page by vertical distance, preferring the block
   directly above (figures) or below (tables) per common layout; ties → nearest centre.
6. **Section tree**: headings by `level` (if missing, inferred from numbering such as
   `2.3.1` and, post-v1, font size from the text layer). Blocks between headings belong to
   the preceding heading.
7. **Cross references** (`LinkKind::RefersTo`): mentions like "Table 3", "Fig. 2",
   "Figure 2a", "Eq. (4)", "Section 3.1" resolved to the captioned block / formula /
   heading with that number. Language-specific patterns (e.g. "Abb.", "Tab.", "图") come
   from a per-language table; unknown languages fall back to English patterns plus numbers.

## Failure handling

Missing pages break continuation links across the gap; they are never guessed.
`StitchedDocument.missing_pages` lists them for Stage 2 and the job status.

## Post-v1

VLM-assisted judgement for low-confidence links (`blocks.verify`-style), footnote
anchoring, multi-column flow repair across pages.
