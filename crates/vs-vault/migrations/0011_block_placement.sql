-- Where a block sits in the *document*, as Stage 2 stitched it.
--
-- This closes a gap that was visible only by reading two files together. Migration 0005
-- says reading order needs no table of its own because it is "recoverable from
-- `sat_block_content.global_order` together with `furniture`" — and `write_page`, which is
-- the only writer of that satellite, writes `global_order = NULL` and `furniture = false`,
-- because at Stage 1 neither is known yet. Stage 2 knows both and had nowhere to put
-- them, so `READING_ORDER_QUERY` ordered every block by NULL.
--
-- A separate satellite rather than a second `sat_block_content` row: these are different
-- facts, written by different stages at different times, and forcing them into one
-- satellite is exactly what made the second one unwritable. A full second row would also
-- duplicate every block's text and content per run, which for a satellite that holds the
-- page's words is the expensive half of the table.
CREATE TABLE vault.sat_block_placement (
    block_hk      CHAR(64) NOT NULL,
    load_dts      TIMESTAMPTZ NOT NULL,
    hash_diff     CHAR(64) NOT NULL,
    record_source TEXT NOT NULL,
    -- Position in the document's reading order, after multi-column repair.
    global_order  INTEGER NOT NULL,
    -- Running headers, footers and page numbers: ordered with everything else and
    -- excluded from the body.
    furniture     BOOLEAN NOT NULL,
    -- Which column of its page the block sat in, 0-based left to right, NULL on a page
    -- that is not multi-column. Recorded because the repair *reorders* blocks and then
    -- the fact that a page had columns at all is lost: a reader could not tell a repaired
    -- two-column page from a single-column one, nor which blocks shared a column.
    column_idx    SMALLINT,
    -- How many columns that page had, so `column_idx` can be read without inferring the
    -- page's shape from the blocks that happen to be present.
    page_columns  SMALLINT,
    PRIMARY KEY (block_hk, load_dts)
);

-- The reading-order query: body blocks of a run, in order.
CREATE INDEX sat_block_placement_order_idx ON vault.sat_block_placement (global_order) WHERE NOT furniture;
