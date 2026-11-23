# vs-stitch — Specification

First step of Stage 2. Turns the independent `PageResult`s of one document into one
`StitchedDocument`. Stage 1 works per page for parallelism; stitching repairs everything
that page boundaries break. It is **CPU-only and deterministic** (no model calls in v1).
System spec: [`docs/SPEC.md`](../../docs/SPEC.md).

## Status

Implemented: global reading order, furniture detection, paragraph and table continuation
links, caption pairing, the section tree, and multilingual cross-references (9 language
label tables). 12 unit tests pass.

Also implemented: **intra-page structure** (`page_layout`) — multi-column reading-order
repair, geometric enclosure, and form key/value pairing. Before this a page carried a
single linear `reading_order` and nothing else, so a two-column paper, a form and a flat
memo were structurally indistinguishable.

Also implemented since: **caption pairing on alignment** (horizontal overlap and the gap
between boxes, with the figure-below / table-above convention breaking ties and stated in
the confidence — centre distance alone paired a caption with the wrong figure on a page
with two figures side by side); **footnote anchoring** (`FootnoteOf`, matching the
footnote's marker in the forms a page uses — a superscript or a bracketed digit, never a
bare one, so "Table 3" does not anchor footnote 3); **heading levels from the text
layer's type sizes** when nothing numbered them, so an unnumbered document stops having a
one-level section tree; **a consistent narrow gutter** read as two columns, because a
journal page's gutter is tighter than `MIN_COLUMN_GAP` and the question that distinguishes
it from a ragged column is "is there an x nothing crosses?"; and **key/value pairing that
does not depend on the classifier agreeing** the page is a form, keeps both values when a
label has two (a date range, a split amount), and tells a label from a value by the colon
rather than by shape.

Column membership survives the repair: a block carries which column of its page it was in
and how many that page had, because reordering for multi-column flow otherwise erases the
fact that the page had columns at all — and the vault stores it
(`vault.sat_block_placement`), which is also where the document-level reading order lives
now. Stage 1 writes a block as one page saw it and cannot know either.

Remaining:
- [ ] **Nothing here is validated against ground truth.** Column bands, caption pairing,
      footnote anchoring, key/value pairing and the link confidences (0.4–0.9, hand-set)
      are all geometry and heuristics that behave correctly on the cases their tests
      describe. Whether they are *right* on a corpus of real pages is unmeasured, because
      `vs-adapt` scores block output rather than links and no labelled link set exists.
      The synthetic corpus could be extended to label captions and columns, which is the
      cheapest next step and still synthetic.
- [ ] Enclosure finds nothing on a typical page, because most layout models emit
      non-overlapping sibling regions. That is the correct answer for those pages, but it
      means the relation is only exercised by models that nest.
- [ ] A sidebar reads as a column of its own, which is the correct reading for ordering
      and the wrong one for a reader that wants the main flow alone. Telling them apart
      needs a notion of "main" that geometry does not supply.

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

## Intra-page structure (`page_layout`)

Derived from geometry alone, like the rest of this crate, and all thresholds are
fractions of the page so none of them depend on DPI.

**Column flow.** A page is cut into horizontal bands at every block at least 60 % of the
page wide, and only the blocks *between* those are grouped into columns and read
column-major. The bands matter: a two-column paper is not two columns, it is a spanning
title, then columns, then often a spanning figure, then more columns. Clustering the whole
page at once puts the title in the left column and drags everything after it out of order.
The repair runs **before** sections, continuations and cross-references, all of which read
`global_order`, and only when a page really has two or more columns *and* the result
differs from the model's own order — the model is usually right, and overriding it on a
single-column page could only make things worse.

**Enclosure.** A block whose box lies 85 % or more inside another, on the same page, is
linked to its **smallest** enclosing block, so a caption inside a figure inside a column
attaches to the figure. Not 1.0 coverage, because a caption printed over a figure's bottom
edge is enclosed in intent even when a few per cent of its box pokes out.

**Key/value.** On form-like pages only (`Form`, `Invoice`, `Receipt`), a label is paired
with the nearest value to its right on the same line, or failing that directly below.
Side-by-side first because it is far more common, and stacked pairs only read as pairs
when nothing sits to the right. A label is a short run of text ending in a colon, or a
short title-cased run; a sentence ending in a full stop is not a form field, which is the
cheapest way to tell a paragraph from a label. A "value" that is itself a label is a
column header, not a pair, and is discarded.

Gating on the **page's** type rather than the document's is deliberate: a contract with an
invoice stapled to the back pairs fields only on the invoice pages. Run everywhere, this
would pair every heading with the paragraph under it.

## Post-v1

VLM-assisted judgement for low-confidence links — which would make this crate
non-deterministic, so it stays a deliberate v1 exclusion. `blocks.verify` now scores
agreement between two parsers arithmetically, which is the deterministic half of the same
idea and runs a step earlier, on blocks rather than links. Footnote anchoring and
multi-column flow repair are now scheduled; see `## Status`.
