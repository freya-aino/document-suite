-- Document-level type, which nothing previously persisted.
--
-- Its own satellite rather than more columns on `sat_document_meta`, because the two
-- have different sources and different rates of change: the meta row is written once by
-- ingest from the upload, while the type is settled in Stage 2 and changes whenever a
-- document is reprocessed with a better classifier. Keeping them apart means a
-- reclassification does not have to restate the filename and size, and the history of
-- how a document's type changed stays readable on its own.
CREATE TABLE vault.sat_document_type (
    document_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    run_id UUID NOT NULL,
    doc_type TEXT NOT NULL,
    -- Share of the evidence behind the type, not a probability.
    confidence REAL NOT NULL,
    -- `caller`, `model` or `page_vote`: which of these it was changes how much a reader
    -- should trust it, so it is stored rather than inferred from the confidence.
    source TEXT NOT NULL,
    PRIMARY KEY (document_hk, load_dts)
);

-- Privileges come from the ALTER DEFAULT PRIVILEGES set in 0003.
