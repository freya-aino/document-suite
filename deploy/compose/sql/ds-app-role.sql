-- The runtime role, as a deploy step.
--
-- Migration 0003 refuses to run without it, by design: the grants it makes are the
-- difference between the worker being able to write and being able to delete. This file
-- is the one definition of the role — `.github/workflows/ci.yml` and
-- `deploy/compose/docker-compose.yml` both run it, so the two paths cannot drift.
--
-- Idempotent: re-running is a no-op, which is what makes `docker compose up` repeatable.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ds_app') THEN
    CREATE ROLE ds_app LOGIN PASSWORD 'ds_app';
  END IF;
END
$$;
