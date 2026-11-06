-- API support: idempotency keys, job state history, region evidence.
-- Default privileges from 0003 apply to the new tables (same owner); grants are
-- repeated explicitly so the runtime role works even if defaults were changed.

CREATE TABLE ops.idempotency_keys (
    tenant_id   TEXT NOT NULL,
    key         TEXT NOT NULL,
    response    JSONB NOT NULL,            -- { status, body }
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, key)
);
CREATE INDEX idempotency_keys_created_idx ON ops.idempotency_keys (created_at);

-- When a job entered each state (for `timings` in the job status).
CREATE TABLE ops.job_state_log (
    job_id      UUID NOT NULL REFERENCES ops.jobs(job_id),
    state       TEXT NOT NULL,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX job_state_log_job_idx ON ops.job_state_log (job_id, at);

CREATE INDEX jobs_document_idx ON ops.jobs (tenant_id, document_hk, created_at DESC);

GRANT SELECT, INSERT, UPDATE ON ops.idempotency_keys, ops.job_state_log TO vs_app;
-- Expired idempotency keys are removed by the writer.
GRANT DELETE ON ops.idempotency_keys TO vs_app;

-- Visual evidence (`Evidence::Region`) points at a page, not a block.
ALTER TABLE vault.lnk_assertion_evidence
    ALTER COLUMN block_hk DROP NOT NULL,
    ADD COLUMN page_hk CHAR(64),
    ADD CONSTRAINT lnk_assertion_evidence_target CHECK (block_hk IS NOT NULL OR page_hk IS NOT NULL);
