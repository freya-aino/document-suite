-- Read scoping inside an organization, job leases, and token revocation.
--
-- Why these three tables are in `ops` and not in `vault`: every one of them is state
-- that must be able to *change* — a document is re-shared, a lease is taken and
-- released, a session is revoked, a person asks to be erased. The raw vault is
-- insert-only at the database level (0003 grants it no UPDATE or DELETE, decision D5),
-- and a store that never forgets cannot answer an erasure request or a revoked share.
-- So access control is operational state next to the jobs, which is also the only other
-- place personal data lives — one surface to erase instead of two (docs/SPEC.md §5.6).

-- ───────────── read scoping ─────────────

-- Who uploaded a document, and who else in the organization may read it.
--
-- One row per (tenant, document). Written by the ingest stage, which is the first point
-- at which the document hash key exists; updated through the access routes.
--
-- A document with **no row here** is readable by the whole organization. That is not a
-- default anyone chose: it is what a document uploaded before this migration means,
-- since nothing recorded who owned it, and inventing an owner for it would lock a
-- tenant out of its own history.
CREATE TABLE ops.document_access (
    tenant_id         TEXT NOT NULL,
    document_hk       CHAR(64) NOT NULL,
    -- The uploader's Keycloak `sub`. NULL when the job carried no owner (a spec written
    -- before authentication existed), which reads as "the organization's".
    owner_subject     TEXT,
    owner_department  TEXT,
    -- The uploader's workspaces at upload time, which is what `visibility = 'workspace'`
    -- is evaluated against. Snapshotted deliberately: a document shared with a workspace
    -- stays shared with that workspace even after the uploader leaves it.
    workspaces        TEXT[] NOT NULL DEFAULT '{}',
    visibility        TEXT NOT NULL CHECK (visibility IN ('private', 'department', 'workspace', 'organization')),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, document_hk)
);
-- Erasure and "everything this person owns" both ask by subject.
CREATE INDEX document_access_owner_idx ON ops.document_access (tenant_id, owner_subject);

-- Shares beyond the document's own visibility: one row per grantee.
--
-- Separate from `visibility` because they answer different questions — visibility is how
-- wide the document is open, a grant is one named exception — and because a grant is
-- revoked by deleting a row, which is the operation an erasure request needs.
CREATE TABLE ops.document_grants (
    tenant_id    TEXT NOT NULL,
    document_hk  CHAR(64) NOT NULL,
    grantee_kind TEXT NOT NULL CHECK (grantee_kind IN ('subject', 'department', 'workspace')),
    grantee_id   TEXT NOT NULL,
    -- Who granted it, for an audit question later. Personal data, and erased with the
    -- rest of a subject's rows.
    granted_by   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, document_hk, grantee_kind, grantee_id)
);

-- ───────────── job ownership, leases and retention ─────────────

ALTER TABLE ops.jobs
    -- The caller's subject, lifted out of `spec` so a job can be scoped, counted and
    -- erased without reading JSONB. `spec.owner` stays the record of what was asked;
    -- this is the column that is indexed and scrubbed.
    ADD COLUMN owner_subject TEXT,
    -- Which process is carrying this job, and until when. A crash leaves the lease
    -- behind; it expires on its own, and the next resume sweep claims the job.
    ADD COLUMN lease_owner      TEXT,
    ADD COLUMN lease_expires_at TIMESTAMPTZ,
    -- Resume attempts, so a job that crashes the process that picks it up cannot be
    -- picked up forever.
    ADD COLUMN resumes          INTEGER NOT NULL DEFAULT 0,
    -- When the personal data in this row was removed. NULL = still present.
    ADD COLUMN owner_erased_at  TIMESTAMPTZ;

-- Backfill: every job written before this migration carries its owner in `spec`.
UPDATE ops.jobs SET owner_subject = spec -> 'owner' ->> 'subject' WHERE spec -> 'owner' ->> 'subject' IS NOT NULL;

-- The resume sweep's query: unfinished jobs whose lease has lapsed, oldest first.
CREATE INDEX jobs_resumable_idx ON ops.jobs (lease_expires_at)
    WHERE state NOT IN ('done', 'partial', 'failed');
-- Erasure and per-person job listing.
CREATE INDEX jobs_owner_idx ON ops.jobs (tenant_id, owner_subject);

-- ───────────── token revocation ─────────────

-- Tokens that must stop being accepted before they expire.
--
-- Signatures are verified locally against the realm's published keys (D10), which is
-- what keeps Keycloak off the path of every request — and is also why a token stays good
-- for its `exp` after the session behind it ends. This table is the exception list: it
-- is written by Keycloak's own back-channel logout and by the admin route, and read on
-- every request from a small in-process cache.
--
-- `kind` says what `id` is:
--   jti     — one token
--   sid     — one session (Keycloak's `sid` claim; what a back-channel logout names)
--   subject — every token of one person issued before `not_before`
CREATE TABLE ops.revoked_tokens (
    kind        TEXT NOT NULL CHECK (kind IN ('jti', 'sid', 'subject')),
    id          TEXT NOT NULL,
    -- NULL for a revocation that is not an organization's to scope (a realm-wide logout).
    tenant_id   TEXT,
    -- Tokens issued (`iat`) before this instant are refused. `now()` for a logout.
    not_before  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- When this row stops mattering, because every token it could match has expired.
    -- The purge reads it; a row without one would be kept forever.
    expires_at  TIMESTAMPTZ NOT NULL,
    reason      TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, id)
);
CREATE INDEX revoked_tokens_expiry_idx ON ops.revoked_tokens (expires_at);

-- ───────────── privileges ─────────────

-- 0003's default privileges already grant SELECT, INSERT and UPDATE on new `ops` tables.
-- DELETE is granted here, per table, because these are the three places where removing a
-- row is the operation itself: unsharing a document, erasing a person, and purging a
-- revocation whose tokens have all expired.
GRANT DELETE ON ops.document_access, ops.document_grants, ops.revoked_tokens TO vs_app;
