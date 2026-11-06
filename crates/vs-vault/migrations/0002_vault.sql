-- Data Vault 2.0 raw vault. Insert-only.

CREATE SCHEMA IF NOT EXISTS vault;

-- ───────────── Hubs ─────────────
CREATE TABLE vault.hub_document (
    document_hk     CHAR(64) PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    content_sha256  CHAR(64) NOT NULL,
    load_dts        TIMESTAMPTZ NOT NULL,
    record_source   TEXT NOT NULL,
    UNIQUE (tenant_id, content_sha256)
);

CREATE TABLE vault.hub_page (
    page_hk        CHAR(64) PRIMARY KEY,
    document_hk    CHAR(64) NOT NULL,
    page_no        INTEGER NOT NULL,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

CREATE TABLE vault.hub_block (
    block_hk       CHAR(64) PRIMARY KEY,
    page_hk        CHAR(64) NOT NULL,
    run_id         UUID NOT NULL,
    block_index    INTEGER NOT NULL,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

CREATE TABLE vault.hub_entity (
    entity_hk      CHAR(64) PRIMARY KEY,
    tenant_id      TEXT NOT NULL,
    canonical_key  TEXT NOT NULL,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

CREATE TABLE vault.hub_assertion (
    assertion_hk   CHAR(64) PRIMARY KEY,
    run_id         UUID NOT NULL,
    predicate      TEXT NOT NULL,
    arg_keys       TEXT NOT NULL,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

CREATE TABLE vault.hub_run (
    run_hk         CHAR(64) PRIMARY KEY,
    run_id         UUID NOT NULL UNIQUE,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

CREATE TABLE vault.hub_dag (
    dag_hk         CHAR(64) PRIMARY KEY,
    dag_id         TEXT NOT NULL,
    version        TEXT NOT NULL,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

CREATE TABLE vault.hub_model (
    model_hk       CHAR(64) PRIMARY KEY,
    model_id       TEXT NOT NULL,
    revision       TEXT NOT NULL,
    load_dts       TIMESTAMPTZ NOT NULL,
    record_source  TEXT NOT NULL
);

-- ───────────── Links ─────────────
CREATE TABLE vault.lnk_document_page (
    lnk_hk CHAR(64) PRIMARY KEY, document_hk CHAR(64) NOT NULL, page_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE TABLE vault.lnk_page_block (
    lnk_hk CHAR(64) PRIMARY KEY, page_hk CHAR(64) NOT NULL, block_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE TABLE vault.lnk_run_document (
    lnk_hk CHAR(64) PRIMARY KEY, run_hk CHAR(64) NOT NULL, document_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE TABLE vault.lnk_block_dag_model (
    lnk_hk CHAR(64) PRIMARY KEY, block_hk CHAR(64) NOT NULL, dag_hk CHAR(64) NOT NULL, model_hk CHAR(64),
    node_id TEXT NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE TABLE vault.lnk_block_block (
    lnk_hk CHAR(64) PRIMARY KEY, from_block_hk CHAR(64) NOT NULL, to_block_hk CHAR(64) NOT NULL,
    link_kind TEXT NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE TABLE vault.lnk_mention (
    lnk_hk CHAR(64) PRIMARY KEY, block_hk CHAR(64) NOT NULL, entity_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE INDEX lnk_mention_entity_idx ON vault.lnk_mention (entity_hk);
CREATE TABLE vault.lnk_assertion_arg (
    lnk_hk CHAR(64) PRIMARY KEY, assertion_hk CHAR(64) NOT NULL, entity_hk CHAR(64) NOT NULL,
    role TEXT NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);
CREATE INDEX lnk_assertion_arg_entity_idx ON vault.lnk_assertion_arg (entity_hk);
CREATE TABLE vault.lnk_assertion_evidence (
    lnk_hk CHAR(64) PRIMARY KEY, assertion_hk CHAR(64) NOT NULL, block_hk CHAR(64) NOT NULL,
    load_dts TIMESTAMPTZ NOT NULL, record_source TEXT NOT NULL
);

-- ───────────── Satellites ─────────────
CREATE TABLE vault.sat_document_meta (
    document_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    external_ref TEXT, filename TEXT, size_bytes BIGINT, format TEXT, language_hint TEXT, page_count INTEGER,
    PRIMARY KEY (document_hk, load_dts)
);
CREATE TABLE vault.sat_run (
    run_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    dag_list_version TEXT, profile TEXT, priority TEXT, state TEXT, started_at TIMESTAMPTZ, finished_at TIMESTAMPTZ,
    PRIMARY KEY (run_hk, load_dts)
);
CREATE TABLE vault.sat_page_profile (
    page_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    run_id UUID NOT NULL, profile JSONB NOT NULL, dag_ref TEXT NOT NULL, rule_id TEXT,
    render_width_px INTEGER, render_height_px INTEGER,
    PRIMARY KEY (page_hk, load_dts)
);
CREATE TABLE vault.sat_block_content (
    block_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    kind TEXT NOT NULL, bbox JSONB NOT NULL, reading_order INTEGER NOT NULL, global_order INTEGER,
    text TEXT NOT NULL, content JSONB NOT NULL, level SMALLINT, confidence REAL NOT NULL, furniture BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (block_hk, load_dts)
);
CREATE TABLE vault.sat_block_provenance (
    block_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    provenance JSONB NOT NULL,
    PRIMARY KEY (block_hk, load_dts)
);
CREATE TABLE vault.sat_entity_label (
    entity_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    name TEXT NOT NULL, type_id TEXT, raw_type TEXT, aliases JSONB NOT NULL DEFAULT '[]', review_status TEXT NOT NULL DEFAULT 'pending',
    PRIMARY KEY (entity_hk, load_dts)
);
CREATE TABLE vault.sat_assertion_detail (
    assertion_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    predicate_raw TEXT NOT NULL, predicate_id TEXT, kind TEXT NOT NULL, qualifiers JSONB NOT NULL, literals JSONB NOT NULL,
    confidence REAL NOT NULL, verified BOOLEAN NOT NULL, provenance JSONB NOT NULL,
    PRIMARY KEY (assertion_hk, load_dts)
);
CREATE TABLE vault.sat_mention (
    lnk_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, sub_seq INTEGER NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    run_id UUID NOT NULL, char_start INTEGER NOT NULL, char_end INTEGER NOT NULL, surface TEXT NOT NULL, confidence REAL NOT NULL,
    PRIMARY KEY (lnk_hk, load_dts, sub_seq)
);
CREATE TABLE vault.sat_evidence (
    lnk_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, sub_seq INTEGER NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    char_start INTEGER, char_end INTEGER, bbox JSONB,
    PRIMARY KEY (lnk_hk, load_dts, sub_seq)
);
CREATE TABLE vault.sat_stitch_link (
    lnk_hk CHAR(64) NOT NULL, load_dts TIMESTAMPTZ NOT NULL, hash_diff CHAR(64) NOT NULL, record_source TEXT NOT NULL,
    run_id UUID NOT NULL, confidence REAL NOT NULL,
    PRIMARY KEY (lnk_hk, load_dts)
);

-- Latest run per document (current view used by downstream consumers).
CREATE VIEW vault.v_current_run AS
SELECT DISTINCT ON (l.document_hk) l.document_hk, r.run_id, l.load_dts
FROM vault.lnk_run_document l
JOIN vault.hub_run r ON r.run_hk = l.run_hk
ORDER BY l.document_hk, l.load_dts DESC;
