-- The graph layer that previously existed only in the separate graph store.
--
-- Decision D6: the vault is both the full history and the current graph projection, so
-- these are the tables that were missing to make the vault a complete `DocGraph` target:
-- the section tree, section membership, and the entity-to-entity shortcut edge.
--
-- Reading order (`Next`) is deliberately absent: it is already recoverable from
-- `sat_block_content.global_order` together with `furniture`, so a link table would only
-- be a second, divergeable copy of it.

-- ───────────── Sections ─────────────
-- A section is identified by the heading block that opens it (section_hk =
-- H(heading_block_hk)), so it is run-scoped through that block and needs no run column
-- of its own to stay unique across reprocessing.
CREATE TABLE vault.hub_section (
    section_hk        CHAR(64) PRIMARY KEY,
    document_hk       CHAR(64) NOT NULL,
    run_id            UUID NOT NULL,
    heading_block_hk  CHAR(64) NOT NULL,
    load_dts          TIMESTAMPTZ NOT NULL,
    record_source     TEXT NOT NULL
);
CREATE INDEX hub_section_run_idx ON vault.hub_section (document_hk, run_id);

CREATE TABLE vault.sat_section (
    section_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    title TEXT NOT NULL, level SMALLINT NOT NULL,
    PRIMARY KEY (section_hk, load_dts)
);

-- Section tree (`has_section`). A top-level section has parent_section_hk IS NULL, which
-- is how the document-to-section edge is represented without a second table.
CREATE TABLE vault.lnk_section_section (
    lnk_hk CHAR(64) PRIMARY KEY,
    parent_section_hk CHAR(64), child_section_hk CHAR(64) NOT NULL,
    document_hk CHAR(64) NOT NULL, run_id UUID NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE INDEX lnk_section_section_child_idx ON vault.lnk_section_section (child_section_hk);
CREATE INDEX lnk_section_section_parent_idx ON vault.lnk_section_section (parent_section_hk);

-- Section membership (`contains`): which blocks make up a section's body.
CREATE TABLE vault.lnk_section_block (
    lnk_hk CHAR(64) PRIMARY KEY, section_hk CHAR(64) NOT NULL, block_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE INDEX lnk_section_block_section_idx ON vault.lnk_section_block (section_hk);
CREATE INDEX lnk_section_block_block_idx ON vault.lnk_section_block (block_hk);

-- ───────────── Entity shortcut edge ─────────────
-- `related`: subject → object of a binary assertion, materialised so entity-to-entity
-- traversal is one index lookup instead of a self-join through lnk_assertion_arg. The
-- assertion remains the evidence-bearing record; this is a derived edge and carries the
-- assertion_hk so a traversal can always get back to it.
CREATE TABLE vault.lnk_entity_entity (
    lnk_hk CHAR(64) PRIMARY KEY,
    from_entity_hk CHAR(64) NOT NULL, to_entity_hk CHAR(64) NOT NULL,
    assertion_hk CHAR(64) NOT NULL, run_id UUID NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE INDEX lnk_entity_entity_from_idx ON vault.lnk_entity_entity (from_entity_hk);
CREATE INDEX lnk_entity_entity_to_idx ON vault.lnk_entity_entity (to_entity_hk);
CREATE INDEX lnk_entity_entity_run_idx ON vault.lnk_entity_entity (run_id);

CREATE TABLE vault.sat_related (
    lnk_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    predicate_id TEXT NOT NULL, confidence REAL NOT NULL,
    PRIMARY KEY (lnk_hk, load_dts)
);

-- ───────────── Citations ─────────────
-- `cites`: a reference-list entry, as an entity, cited from a block. vs-extract already
-- produces a `cites` predicate; this gives the graph edge somewhere to live so citation
-- traversal does not have to go through assertions.
CREATE TABLE vault.lnk_block_entity_cites (
    lnk_hk CHAR(64) PRIMARY KEY, block_hk CHAR(64) NOT NULL, entity_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE INDEX lnk_block_entity_cites_entity_idx ON vault.lnk_block_entity_cites (entity_hk);

-- No GRANTs needed: 0003 set ALTER DEFAULT PRIVILEGES on the `vault` schema, so these
-- tables get SELECT and INSERT for `vs_app` on creation — and, deliberately, nothing
-- else. The raw vault stays insert-only at the database level.
