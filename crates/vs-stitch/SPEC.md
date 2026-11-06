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
paragraph split over two pages) is produced on demand:

- `StitchedDocument::continuation_chains()` → the chains of blocks joined by
  `ContinuesOn` links (document order, ≥ 2 blocks each);
- `StitchedDocument::text_of(chain)` → the texts joined with a space; a part ending in a
  hyphen (`-`, U+2010, soft hyphen) followed by a part starting with a lowercase letter is
  de-hyphenated (joined without the hyphen). Unknown keys are skipped.

`Link { kind, from, to, confidence, header_repeated }`; `header_repeated` is
`#[serde(default)]` and only meaningful for `TableContinues`.

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
   the HTML). If the second table's first row has the same cell texts (tags stripped,
   whitespace normalised, case-insensitive) as the first table's, the link gets
   `header_repeated = true` and confidence 0.9, else 0.85.
5. **Caption pairing** (`LinkKind::CaptionOf`): each caption links to the nearest
   figure/table/chart on the same page by vertical distance, preferring the block
   directly above (figures) or below (tables) per common layout; ties → nearest centre.
6. **Section tree**: headings by `level` (if missing, inferred from numbering such as
   `2.3.1` and, post-v1, font size from the text layer). Blocks between headings belong to
   the preceding heading.
7. **Cross references** (`LinkKind::RefersTo`, module `xref`): mentions in body text
   blocks (`Paragraph`, `List`, `Footnote`; furniture excluded) are resolved to a target
   block. The link goes `from` the mentioning block `to` the target.
   - **Mentions**: a label word followed by an optional `.`, optional whitespace and a
     number: `3`, `2a` (one sub-figure letter), `3.1.2`, `(4)`. Alphabetic labels match
     case-insensitively at word boundaries ("configure 3" does not match "Fig"); CJK labels
     match anywhere ("图2", "第3节"). Labels come from a per-language table keyed by the
     page profile's primary language subtag (`en`, `de`, `fr`, `es`, `it`, `pt`, `nl`,
     `zh`, `ja`); every language also gets the English labels, unknown languages get
     English only. Longer labels win ("figure" over "fig").
   - **Figures / tables**: candidates are `Caption` blocks whose text *starts* with a label
     of the same target and the same number ("Figure 2a" also matches caption "Figure 2").
     Without caption candidates, `Table`/`Figure`/`Chart` blocks whose own text starts with
     the label are used. A caption with a `CaptionOf` link is replaced by the captioned
     block, so the link points at the figure/table itself.
   - **Equations**: `Formula` blocks whose LaTeX (or text) has `\tag{4}` / `\tag*{4}` or a
     trailing `(4)`. If none carries the number, the nearest formula in reading order is
     taken with confidence 0.4.
   - **Sections**: `Heading`/`Title` blocks whose numbering token (`3.1` in "3.1 Scope" or
     "3.1. Scope") equals the number.
   - Confidence 0.8 when exactly one candidate matches, 0.5 when several do and the one
     nearest in global reading order was chosen. No self-links; one link per
     (source, target) pair. Unresolvable mentions produce nothing.

## Failure handling

Missing pages break continuation links across the gap; they are never guessed.
`StitchedDocument.missing_pages` lists them for Stage 2 and the job status.

## Post-v1

VLM-assisted judgement for low-confidence links (`blocks.verify`-style), footnote
anchoring, multi-column flow repair across pages.
