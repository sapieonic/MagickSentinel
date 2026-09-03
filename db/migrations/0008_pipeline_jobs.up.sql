-- What the pipeline's scheduled jobs need from the schema, and nothing else.
--
-- Two things, both discovered while wiring `server/pipeline` to a real database:
--
-- 1. A nightly job has to know which tenants to visit, and under row-level security
--    it cannot find out. The `tenants_self` policy (0002) is `id = sentinel_tenant()`,
--    so the query that would produce the list of tenant contexts is itself a query
--    that needs one. This is the same shape of problem 0005 solved for the gateway's
--    three bootstrap lookups, and it gets the same answer: a narrow SECURITY DEFINER
--    function rather than a loosened policy. Loosening `tenants_self` would let every
--    role that can reach the database enumerate the customer list; a function that
--    returns three columns to one role is auditable in a way a policy is not.
--
-- 2. The retention purge scans by age, and neither table has an index for it. Without
--    these two, the nightly job sequentially scans `media_segments` — the largest
--    table in the schema by two orders of magnitude, one row per second of every
--    channel of every call — once per tenant per night.

BEGIN;

-- Tenant ids and the two retention periods. Deliberately not "everything about a
-- tenant": no policy blob, no budget, no name. A job that needs more than this reads
-- it under that tenant's own context, where the policies apply.
--
-- The retention periods are returned rather than assumed because they are OPEN-6 and
-- unsettled (the schema's 30 and 365 are documented placeholders). The pipeline reads
-- them per tenant on every run, so the day the customer answers, the answer takes
-- effect without a code change.
--
-- SECURITY DEFINER with a pinned search_path: the function runs as the schema owner,
-- so an injected `search_path` must not be able to point `tenants` at another table.
CREATE OR REPLACE FUNCTION sentinel_pipeline_tenants()
  RETURNS TABLE (
    tenant_id                 uuid,
    audio_retention_days      int,
    transcript_retention_days int,
    timezone                  text
  )
  LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS
$$
  SELECT id, audio_retention_days, transcript_retention_days, timezone
    FROM tenants
   ORDER BY id
$$;

-- EXECUTE on a new function is granted to PUBLIC by default, which would hand the
-- customer list to `sentinel_app` and to anything else with a login. Revoke first,
-- then grant to the one role that has a reason: the pipeline, whose nightly jobs are
-- the only tenant-agnostic work in the system. The gateway is deliberately not
-- granted it — every gateway request already knows its tenant from a verified token.
REVOKE ALL ON FUNCTION sentinel_pipeline_tenants() FROM PUBLIC;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sentinel_pipeline') THEN
    GRANT EXECUTE ON FUNCTION sentinel_pipeline_tenants() TO sentinel_pipeline;
  END IF;
END $$;

-- Retention scans. `received_at` rather than the call's start, because that is what
-- the purge compares against: a segment's own arrival is when its retention clock
-- starts, and it is also the day its object key is partitioned under.
--
-- Plain CREATE INDEX, not CONCURRENTLY, because this file runs inside a transaction
-- like every other migration here. On a table that is already large, build these by
-- hand with CONCURRENTLY first — the migration then finds them present and does
-- nothing.
CREATE INDEX IF NOT EXISTS media_segments_tenant_received_idx
  ON media_segments (tenant_id, received_at);

CREATE INDEX IF NOT EXISTS transcripts_tenant_created_idx
  ON transcripts (tenant_id, created_at);

COMMIT;
