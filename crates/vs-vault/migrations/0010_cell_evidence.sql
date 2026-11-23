-- Evidence that points at one cell of a table.
--
-- A table reaches extraction as HTML and its plain-text projection is the cells run
-- together, so a character span into that projection is the wrong instrument for citing a
-- number: `92.1` twice in a results table gives two identical spans, and the citation
-- points at whichever came first. Row and column are what make a value citable — and they
-- are what `vault.hub_cell` is already keyed by, so this evidence resolves to a row that
-- exists.
--
-- Both nullable, like `char_start`/`char_end` and `bbox` beside them: one evidence row
-- carries exactly one of the three shapes (span, region, cell).
ALTER TABLE vault.sat_evidence
    ADD COLUMN row_idx INTEGER,
    ADD COLUMN col_idx INTEGER;

-- One shape per row, enforced rather than described: a row with a span *and* a cell would
-- be two citations in one, and a reader joining it would get both.
ALTER TABLE vault.sat_evidence
    ADD CONSTRAINT sat_evidence_one_shape CHECK (
        (char_start IS NOT NULL)::int + (bbox IS NOT NULL)::int + (row_idx IS NOT NULL)::int <= 1
    );

-- Finding the assertions that cite a given cell, which is the query this exists for.
CREATE INDEX sat_evidence_cell_idx ON vault.sat_evidence (row_idx, col_idx) WHERE row_idx IS NOT NULL;
