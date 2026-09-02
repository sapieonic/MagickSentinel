-- Application role. NOBYPASSRLS is the point: even a query that forgets its tenant
-- filter cannot read another tenant's rows.
BEGIN;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_app') THEN
    CREATE ROLE sentinel_app LOGIN NOBYPASSRLS;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_pipeline') THEN
    CREATE ROLE sentinel_pipeline LOGIN NOBYPASSRLS;
  END IF;
END $$;

GRANT USAGE ON SCHEMA public TO sentinel_app, sentinel_pipeline;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public
  TO sentinel_app, sentinel_pipeline;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public
  TO sentinel_app, sentinel_pipeline;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO sentinel_app, sentinel_pipeline;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT USAGE, SELECT ON SEQUENCES TO sentinel_app, sentinel_pipeline;

COMMIT;
