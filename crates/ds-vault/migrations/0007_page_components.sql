-- Components *inside* a block: table cells and text found in images.
--
-- A table previously reached the vault as one HTML string in `sat_block_content`, and a
-- figure as one description. Both are fine for retrieval and useless for citation: a
-- question answered by one number in a results table could only be traced to "that
-- table", and a label inside a chart was unreachable even when the same term appeared in
-- the body text.
--
-- Both are run-scoped through their block (`block_hk` already contains a `run_id`), so
-- neither needs a run column to stay unique across reprocessing. They carry one anyway,
-- because the cheapest way to find a run's components is a single indexed predicate
-- rather than a join back through the block.

-- ───────────── Table cells ─────────────
CREATE TABLE vault.hub_cell (
    cell_hk     CHAR(64) PRIMARY KEY,
    block_hk    CHAR(64) NOT NULL,
    run_id      UUID NOT NULL,
    row_idx     INTEGER NOT NULL,
    col_idx     INTEGER NOT NULL,
    load_dts    TIMESTAMPTZ NOT NULL,
    record_source TEXT NOT NULL,
    UNIQUE (block_hk, row_idx, col_idx)
);
CREATE INDEX hub_cell_block_idx ON vault.hub_cell (block_hk);
CREATE INDEX hub_cell_run_idx ON vault.hub_cell (run_id);

CREATE TABLE vault.sat_cell (
    cell_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    text TEXT NOT NULL,
    header BOOLEAN NOT NULL,
    -- The header-row value for this cell's column, where the table has one. This is what
    -- makes a cell self-describing: "row 3, column 2" means nothing on its own, the same
    -- cell carrying column_header = 'F1' is citable evidence.
    column_header TEXT,
    PRIMARY KEY (cell_hk, load_dts)
);

-- ───────────── Text inside images ─────────────
CREATE TABLE vault.hub_image_text (
    image_text_hk CHAR(64) PRIMARY KEY,
    block_hk      CHAR(64) NOT NULL,
    run_id        UUID NOT NULL,
    -- Position in the model's reading order of the in-image text.
    seq           INTEGER NOT NULL,
    load_dts      TIMESTAMPTZ NOT NULL,
    record_source TEXT NOT NULL,
    UNIQUE (block_hk, seq)
);
CREATE INDEX hub_image_text_block_idx ON vault.hub_image_text (block_hk);
CREATE INDEX hub_image_text_run_idx ON vault.hub_image_text (run_id);

CREATE TABLE vault.sat_image_text (
    image_text_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (image_text_hk, load_dts)
);

-- A fragment of in-image text matching something elsewhere in the document. Exactly one
-- of block_hk and entity_hk is set, which the check enforces: a match with neither
-- target is not a match, and one with both would be two facts in one row.
CREATE TABLE vault.lnk_image_text_match (
    lnk_hk        CHAR(64) PRIMARY KEY,
    image_text_hk CHAR(64) NOT NULL,
    block_hk      CHAR(64),
    entity_hk     CHAR(64),
    -- `block_text` or `entity_name`: how the match was made.
    via           TEXT NOT NULL,
    load_dts      TIMESTAMPTZ NOT NULL,
    record_source TEXT NOT NULL,
    CONSTRAINT lnk_image_text_match_target CHECK ((block_hk IS NULL) <> (entity_hk IS NULL))
);
CREATE INDEX lnk_image_text_match_source_idx ON vault.lnk_image_text_match (image_text_hk);
CREATE INDEX lnk_image_text_match_entity_idx ON vault.lnk_image_text_match (entity_hk);

-- Privileges come from the ALTER DEFAULT PRIVILEGES set in 0003.
