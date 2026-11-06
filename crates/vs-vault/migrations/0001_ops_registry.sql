-- Operational state and registry. Mutable tables, not part of the raw vault.

CREATE SCHEMA IF NOT EXISTS ops;
CREATE SCHEMA IF NOT EXISTS registry;

CREATE TABLE ops.batches (
    batch_id     UUID PRIMARY KEY,
    tenant_id    TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    documents    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE ops.jobs (
    job_id         UUID PRIMARY KEY,
    batch_id       UUID REFERENCES ops.batches(batch_id),
    tenant_id      TEXT NOT NULL,
    document_hk    CHAR(64),
    run_id         UUID NOT NULL,
    state          TEXT NOT NULL,          -- vs_core::JobState
    priority       TEXT NOT NULL,          -- interactive | batch
    spec           JSONB NOT NULL,         -- vs_core::JobSpec
    pages_total    INTEGER,
    pages_done     INTEGER NOT NULL DEFAULT 0,
    pages_failed   INTEGER NOT NULL DEFAULT 0,
    error          TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX jobs_batch_idx ON ops.jobs (batch_id);
CREATE INDEX jobs_state_idx ON ops.jobs (state) WHERE state NOT IN ('done', 'partial', 'failed');

CREATE TABLE ops.page_tasks (
    job_id      UUID NOT NULL REFERENCES ops.jobs(job_id),
    page_no     INTEGER NOT NULL,
    state       TEXT NOT NULL,             -- pending | running | done | failed
    attempts    INTEGER NOT NULL DEFAULT 0,
    dag_ref     TEXT,
    rule_id     TEXT,
    error       TEXT,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (job_id, page_no)
);

-- DAG lists produced by vs-adapt. One active list per tenant (NULL tenant = global).
CREATE TABLE registry.dag_lists (
    version       TEXT PRIMARY KEY,
    tenant_id     TEXT,
    status        TEXT NOT NULL CHECK (status IN ('candidate', 'active', 'retired')),
    body          JSONB NOT NULL,          -- vs_router::DagList
    created_by    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    activated_at  TIMESTAMPTZ
);
CREATE UNIQUE INDEX dag_lists_one_active ON registry.dag_lists (COALESCE(tenant_id, '')) WHERE status = 'active';

-- Ontology grown by type normalisation (vs-extract). Embeddings live in HelixDB.
CREATE TABLE registry.entity_types (
    type_id        TEXT PRIMARY KEY,
    label          TEXT NOT NULL,
    description    TEXT,
    synonyms       TEXT[] NOT NULL DEFAULT '{}',
    parent_type_id TEXT REFERENCES registry.entity_types(type_id),
    review_status  TEXT NOT NULL DEFAULT 'pending' CHECK (review_status IN ('pending', 'approved', 'rejected', 'merged')),
    merged_into    TEXT REFERENCES registry.entity_types(type_id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE registry.relation_types (
    predicate_id   TEXT PRIMARY KEY,
    label          TEXT NOT NULL,
    roles          JSONB NOT NULL DEFAULT '["subject","object"]',
    synonyms       TEXT[] NOT NULL DEFAULT '{}',
    review_status  TEXT NOT NULL DEFAULT 'pending' CHECK (review_status IN ('pending', 'approved', 'rejected', 'merged')),
    merged_into    TEXT REFERENCES registry.relation_types(predicate_id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Seed: scientific frames (vs-extract SPEC.md).
INSERT INTO registry.relation_types (predicate_id, label, roles, review_status) VALUES
    ('result',   'result',   '["method","task","dataset","metric","value","condition"]', 'approved'),
    ('uses',     'uses',     '["agent","resource"]',                                       'approved'),
    ('compares', 'compares', '["subject","baseline","metric","outcome"]',                  'approved'),
    ('proposes', 'proposes', '["author_or_paper","artifact"]',                             'approved'),
    ('cites',    'cites',    '["claim_block","reference"]',                                'approved'),
    ('defines',  'defines',  '["term","definition"]',                                      'approved');
