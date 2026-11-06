-- Runtime role privileges. Migrations run as the schema owner; the API and workers
-- connect as `vs_app`, which must exist (created by the platform / devenv).
-- The raw vault is insert-only at the database level: no UPDATE, DELETE or TRUNCATE.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'vs_app') THEN
        RAISE EXCEPTION 'role vs_app does not exist; create it before migrating (see crates/vs-vault/SPEC.md)';
    END IF;
END
$$;

GRANT USAGE ON SCHEMA ops, registry, vault TO vs_app;

GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA vault TO vs_app;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA ops TO vs_app;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA registry TO vs_app;

-- Tables created by later migrations get the same privileges.
ALTER DEFAULT PRIVILEGES IN SCHEMA vault GRANT SELECT, INSERT ON TABLES TO vs_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA ops GRANT SELECT, INSERT, UPDATE ON TABLES TO vs_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA registry GRANT SELECT, INSERT, UPDATE ON TABLES TO vs_app;

-- Startup check reads the migration history.
GRANT SELECT ON TABLE public._sqlx_migrations TO vs_app;
